# 42: Windowed Metrics Access

Consumers (TUI panels, summary reports, programmatic queries) need
sample-weighted views of every instrument across multiple time
horizons — `now`, `10s`, `1m`, `1h`, lifetime. This document
defines a single windowed-access layer that serves all of them.

The design builds on the existing scheduler's frame coalescing
(see SRD-40 §Coalescing): a higher-cadence reporter already sees
per-window histograms merged losslessly from base-interval
captures. We expose that as a first-class API, wire user-specified
cadences through as canonical presentation, and insert efficient
intermediate buckets automatically.

---

## Motivation

Per-frame post-processing (mean-of-means, average-of-percentiles)
is *biased* — every frame contributes equally regardless of how
many samples it contains. A frame with 1 op and a frame with
10,000 ops contribute the same to a naïve rolling mean.

Proper sample-weighted windowing requires *merging the raw
histograms* (`Histogram::add`) over the window, then computing the
percentile from the merged histogram. The existing scheduler
already does this when a reporter is registered at a slower
interval — we just need a standard way to subscribe, a way to feed
user-visible cadences through, and a way to answer
arbitrary-duration queries.

---

## Canonical Cadences

A run declares a `metrics-cadences` list — the set of windows the
user wants to see everywhere:

```
latency-cadences=10s,1m,10m,10h
```

Semantics:

- Smallest cadence becomes the de-facto "now" bucket (the finest
  granularity the user asked for).
- All declared cadences are guaranteed to be queryable and are
  presented in the order the user wrote them.
- Every consumer (TUI latency panel, summary reports, programmatic
  queries) renders exactly these windows — no hardcoded magic
  numbers elsewhere in the system.

Default: `1s,10s,30s,1m,5m` if not specified. The 1 s layer is
included so `cadence_window(1s)` is always available for short-
term pull-style readers (TUI, programmatic) without requiring an
explicit user declaration — per-layer overhead is negligible
(see `cadence_layout` bench).

### Constraints

- Every declared cadence must be ≥ the scheduler base interval
  (1 s by default). Smaller values are rejected at parse time with
  a diagnostic.
- Cadences need not be integer multiples of each other — the
  scheduler's auto-intermediate layer (below) synthesizes the tree
  of multiples needed to deliver each one efficiently.

---

## Auto-Intermediate Buckets

`10s` → `10h` is ~3,600:1. Accumulating 3,600 base frames into a
single node is memory-heavy and produces a thundering coalesce on
each 10-hour tick. The scheduler breaks the ratio into a
tree of nodes with bounded fan-in.

### Algorithm

Given a sorted list of user cadences `C = [c_1, …, c_n]` and a
maximum fan-in `K` (default 20):

1. Iterate adjacent pairs `(c_i, c_{i+1})`. If
   `c_{i+1} / c_i ≤ K`, no insertion needed.
2. Otherwise insert `ceil(log_K(c_{i+1} / c_i)) − 1` intermediate
   cadences, geometrically spaced. Round each to a
   human-friendly unit (seconds, minutes, hours) when possible.
3. Intermediate cadences are flagged `hidden: true` — they
   exist for accumulation but are not presented to consumers via
   the enumeration API.
4. Re-check ratios; re-insert until every adjacent pair is within
   `K`.

### Example

User declares `10s,1m,10m,10h`:
- 10s → 1m: ratio 6, OK
- 1m → 10m: ratio 10, OK
- 10m → 10h: ratio 60, **insert `1h`** (hidden) so each hop is
  10× or 6×
- Final internal tree: `10s → 1m → 10m → 1h* → 10h` (asterisk =
  hidden)

### Tree Construction

The scheduler builds a tree rooted at the base interval. Each
non-root node feeds its coalesced frames to its child. Hidden
nodes have no registered external reporter — they exist solely to
reduce the fan-in of the next visible cadence. Visible nodes
forward to all consumers that asked for that cadence.

### Logging

The realized cadence layout is emitted at `INFO` level when the
tree is constructed as a single one-line summary — declared
cadences inline with hidden intermediates in parentheses, the
trailing `/ N` reporting the active max-fan-in cap (default 20).
Example:

```
INFO metrics: cadences: [1s, 10s, 1m, 10m, (1h), 10h] / 20
```

The parentheses around `1h` mark it as a planner-synthesized
hidden layer (no declared external reporter; feeds coalescing
only). The user-declared cadences appear bare, in declaration
order with hidden intermediates spliced in at the right positions
so the sequence reads left-to-right ascending. The trailing
`/ 20` carries `max_fan_in` — the cap that drove any insertions
on this run.

### Max fan-in

**Fan-in** is the integer ratio between adjacent cadence layers:
`next.interval / prev.interval`. A `1s → 10s` step has fan-in
`10:1`; a `1s → 5m` step has fan-in `300:1`.

The planner caps adjacent fan-in at `max_fan_in` (default 20)
because upper-layer aggregate accuracy (mean / p99 / histogram
fan-out) degrades non-linearly as the fan-in grows. At 20:1 the
loss is bounded; at 300:1 the rolled-up percentile is effectively
a coin flip. The cap keeps the cost-vs-accuracy trade-off
predictable.

When two declared layers exceed the cap, the planner inserts
geometrically-spaced **hidden intermediate layers** — synthesized
midpoints that keep every adjacent step within the limit. Hidden
layers are not user-visible columns; they exist solely to feed
the coalescing into the next declared layer.

With `max_fan_in = 20`, declared `[1s, 5m]` (300:1) becomes
realized `[1s, (20s), 5m]` — `20s` is hidden, adjacent ratios are
`20:1` and `15:1`, both under the cap.

Constant of truth lives at
`nmbrs_metrics::cadence::DEFAULT_MAX_FAN_IN` in the source. CLI
override surface (`latency-fan-in=`) is reserved but not yet
exposed — the default constant is the single project-wide value
until an operator needs the knob.

```
root (1s base)
  ↓ coalesce 10 frames
  10s ─── emits to consumers
    ↓ coalesce 6 frames
    1m ─── emits to consumers
      ↓ coalesce 10 frames
      10m ─── emits to consumers
        ↓ coalesce 6 frames
        1h* (hidden)
          ↓ coalesce 10 frames
          10h ─── emits to consumers
```

Each tick costs one histogram merge per tree edge, regardless of
the ratio between the outermost visible cadences.

### Histogram rank semantics — discrete data lives only at the base rank

A layer's position in the realized tree is its **rank**: rank 0 is the
smallest cadence (`CadenceReporter::layers()` is ordered smallest-first),
rank 1 the next larger layer, and so on up through any hidden
intermediates to the largest declared cadence. Histogram fidelity is
**not uniform across ranks** — it falls directly out of the streaming
cascade (§"Streaming coalesce semantics" below).

**Where discrete data enters.** The scheduler captures the live
instruments — including the raw per-interval HDR reservoir — at the
**base interval**, and folds each capture into the **smallest cadence's
accumulator** (§"Cadence reporter — single writer"). A base capture is
transient: it is never retained as its own rank, it is merged into
rank 0's prebuffer the moment it arrives. Rank 0 (the smallest declared
cadence) is therefore the **finest retained, queryable rank** — its
window is the smallest coalesced unit the system publishes: exactly one
base capture when the smallest declared cadence equals the base interval
(the default `1s,…` list), or a small merge of base captures when the
smallest declared cadence is larger than the base.

**Where it stops being discrete.** Every coarser rank holds a *merged*
reservoir, never fresh discrete observations. A rank-`n` window is
assembled by folding rank-`(n−1)`'s immutable closed window into
rank-`n`'s prebuffer (`HdrHistogram::add`, §"Combine semantics —
algebraic uniformity"). The merge is lossless *as a distribution* —
`p99` of a rank-3 window is the true p99 over that window's whole span,
not an average-of-percentiles — so an upper-rank histogram is a faithful
roll-up, not the wrong shape. What a higher rank gives up is **temporal
resolution** (no per-base-window detail) and, when adjacent fan-in is
wide, **percentile accuracy of the roll-up** — exactly the loss
§"Max fan-in" caps. Hidden intermediate ranks exist only to keep each
rank-to-rank merge inside the `max_fan_in` accuracy bound; they carry
merged reservoirs like any coarser rank and are simply never surfaced as
columns.

**Consequences.**

- **Discrete-distribution reads target rank 0.** The live query backend
  (`queryapi::live`) draws its series from `declared_cadences().smallest()`
  — rank 0 — the finest rank with per-window distribution data.
  `cadence_window(c)` for a coarser `c` returns that rank's merged
  roll-up by design; `now` peeks the live reservoir directly (finer than
  any rank); `recent_window` / `session_lifetime` ephemerally re-merge
  ranks at read time.
- **The histogram count is rank-independent.** The monotonic
  `cumulative_count` (the cumulative-counter model,
  `notes/cumulative_counter_model.md`) rides *every* rank: it coalesces
  latest-wins up the cascade (each rank carries the count as of its
  window close) and sums only on cross-component aggregate. So
  `rate()` / `increase` over a histogram count is correct read from any
  rank — even though the *distribution* is discrete only at rank 0.

### Streaming coalesce semantics

Coalescing is incremental and pull-free. There is no batched
"build the 10m frame from 600 base frames in one shot" — every
larger cadence is fed by the *immutable closed window* of the
next smaller cadence, one window at a time, in the same tick the
smaller window closes.

On every smallest-cadence tick the scheduler runs the cascade
all the way up the tree as far as it can go in this tick:

1. **Close the smallest window.** The current smallest-cadence
   accumulator is sealed into an immutable snapshot.
2. **Notify subscribers at this cadence.** Every reporter
   registered at the cadence whose window just closed receives
   the snapshot. The snapshot is shared (`Arc`) — readers do not
   clone it.
3. **Fold into the next-larger cadence's prebuffer.** The closed
   snapshot is merged into the prebuffer of the immediately
   larger cadence (declared or hidden). The prebuffer is a
   single accumulating window, not a list of pending frames.
4. **Promote if complete.** If the prebuffer's accumulated
   duration has reached its cadence's interval, the prebuffer is
   sealed into a snapshot and steps 2–4 repeat at this larger
   cadence. If not, accumulation continues; the next promotion
   waits for additional folds in subsequent ticks.

For cadences `1m, 10m`, every 1m tick:
- Close the 1m window → notify 1m subscribers → fold into 10m
  prebuffer.
- 9 times out of 10, the 10m prebuffer is still accumulating;
  no further work happens.
- Once every 10 ticks, the fold completes the 10m prebuffer →
  seal → notify 10m subscribers → fold into the next-larger
  cadence's prebuffer (or stop if 10m is the largest).

### Why this matters

- **Even amortization.** Work per tick is bounded by the depth
  of cascades that complete on this tick, never by the ratio
  between the smallest and largest cadence. The 10h tick is no
  more expensive than the 10s tick — it just propagates one
  more level on the rare ticks when its prebuffer completes.
- **Bounded in-memory state.** Each cadence layer holds at most
  one prebuffer plus one most-recently-closed snapshot. There
  are no pending lists of un-coalesced frames waiting for a
  future tick. Memory at each layer is `O(1)` in the cadence
  ratio, not `O(N)`.
- **Snapshots are immutable once closed.** Subscribers receive
  the same `Arc<Snapshot>` they will see in any subsequent
  query for that cadence's "last completed window" — no risk of
  the snapshot mutating after a reporter has already read it.
- **No tick is ever "skipped" or "deferred."** Every base-tick
  capture flows into every applicable larger cadence's
  prebuffer in the same tick. Larger cadences only differ in
  *when they promote*, never in *which inputs they have seen*.
- **Lifetime is a natural emergent layer.** A cadence whose
  interval ≥ session duration never promotes mid-session — its
  prebuffer simply accumulates every smaller-cadence close
  forever. `cadence_window(LIFETIME)` peeks that prebuffer (a
  read-only clone, like `peek_snapshot`) rather than waiting
  for a promotion that won't come. This makes "lifetime" a
  natural finesse of the cascade mechanism, not a parallel API
  surface.

  But that prebuffer alone *lags* by everything in flight: the
  partial state in every smaller cadence's prebuffer plus the
  unfolded samples in the live instruments themselves.
  Operators asking for "session lifetime" mean *the canonical
  span of the session, right now* — not "the session minus the
  last 30 seconds of in-flight data". To honor that, the
  query mechanism is described next under
  §"Canonical session-lifetime queries".

### Notification dispatch

Snapshots are immutable `Arc<Snapshot>` once closed.
Subscribers are notified *off* the scheduler thread — the
scheduler hands the `Arc` to an async dispatcher and continues
the cascade. Slow subscribers can never stall the cascade.

#### Per-subscriber delivery timeout

Each subscriber has a per-cadence delivery timeout. The
**default is `2 × cadence_interval`** — a 10s cadence allows up
to 20s for delivery, a 1m cadence up to 2m, etc. This 2× grace
factor absorbs normal variation while keeping the failure
threshold proportional to the freshness expectation of the
cadence itself: a 10s subscriber that can't keep up within 20s
is functionally broken.

Subscribers can override the default per-subscription if they
have a hard latency requirement tighter than 2×.

#### Timeout escalation routing

When the async dispatcher exceeds the timeout for a subscriber
it invokes the cadence-manager's registered error callback
exactly once per timeout event. The callback receives:

- the cadence whose delivery timed out,
- a stable subscriber identifier,
- the age of the undelivered snapshot,
- a count of consecutive timeouts for this subscriber/cadence
  pair (so the manager can rate-limit escalation noise).

The cadence manager decides what to do — emit through `diag::warn`
or `diag::error`, escalate through the observer's
`LogLevel::Error` channel, signal session shutdown, or unsubscribe
the offender. The default callback (if no manager registers one)
emits `diag::warn` on the first timeout and `diag::error` on
subsequent consecutive timeouts so the noise is bounded but
nothing is silenced.

Callbacks themselves should still be cheap (`Arc` clone + channel
send is fine; full work belongs on the subscriber's own thread).
The async dispatcher exists to decouple the scheduler from
subscriber latency, not to subsidize expensive in-callback work.

### Canonical session-lifetime queries

A "session lifetime" query must always cover the full canonical
span of the session at the moment of the call — including data
still in flight that has not yet flowed up to the lifetime
prebuffer. The query is implemented by walking the cascade *down*
at read time and peek-merging every intermediate layer with the
live instruments:

1. Read-clone the lifetime cadence's prebuffer (everything
   already folded up).
2. Read-clone every smaller cadence's prebuffer (partials in
   flight that haven't promoted yet).
3. `peek_snapshot` every matched live instrument (samples
   recorded since the last smallest-cadence tick).
4. Merge all of the above, per-metric, into an ephemeral
   combined snapshot. Return it with `interval` set to the
   actual session age and `captured_at = Instant::now()`.

Steps 1–3 are read-only — none of them perturb the cascade
state, drain a reservoir, or shift a tick boundary. The
ephemeral combined snapshot exists only for this query.

`MetricsQuery::session_lifetime(...)` is exposed as a dedicated
mode for ergonomic reasons — every report and operator query for
"the run's totals" routes through it. Internally it is the same
cost-shape as `recent_window`: per-metric merge of the matched
selection only, never a whole-frame combine. The cost rule from
§"Cost rule for `recent_window`" applies unchanged.

This also means there is no need for a separate "cumulative"
data path. Anything historically served by `InProcessMetricsStore`
cumulative views (run totals, lifetime histograms, lifetime rate
calculations) is now served by `session_lifetime` over the same
unified store.

### Lifecycle-boundary close — flush at the end of each scope

Metric paths are scoped by the label set their ingests use.
A for_each iteration of `ann_query` emits under a label set like
`phase=ann_query, profile=label_03, k=10`; the next iteration
uses a different combination and therefore lands in a different
path. Once a scope ends, no further ingests to that path will
ever arrive, and the window should publish *now* rather than
wait for a cadence tick that may never come.

Three scope boundaries are honored explicitly, each calling
`CadenceReporter::close_path(labels)` with the label set that
identifies the closing scope:

1. **End of phase** — the phase executor calls `close_path`
   immediately after the final delta and any validation frame
   are ingested. Label set: the phase's full dimensional labels.
   This is the primary delivery path for most workload metrics.
2. **End of workload** — the runner calls `close_path` after
   all phases in the scenario have completed (phased mode) or
   the single activity has returned (single-activity mode).
   Label set: the activity's root labels (session + activity in
   single-activity mode; session root in phased mode, since
   there's no intermediate `activity=...` dimension).
3. **End of session** — the runner calls `close_path` with the
   session-root labels before tearing down the scheduler or
   reading downstream sinks for the summary. Session-level
   aggregates (rare today but architecturally permitted) flush
   here.

All three use `close_path`, which force-closes every cascade
layer's window for that path, promotes partials up the cascade,
and fans each resulting snapshot out to subscribers via
**blocking `send`** (not `try_send`): lifecycle boundaries are
observable and correctness of downstream sinks matters more
than avoiding back-pressure on the boundary-crossing thread.

Amortization follows naturally — each boundary delivers one
path at a time, paced by the subscriber's ability to consume,
instead of a thundering herd of stale windows arriving at
session shutdown. Short-lived phases (sub-cadence) are no
longer invisible to reports: their data closes at phase-end
rather than waiting for a cadence tick that never comes.

### Session shutdown — belt-and-suspenders flush

`CadenceReporter::shutdown_flush` is the end-of-session safety
net. By the time it runs, `close_path` at phase/workload/
session boundaries should have drained every known path; this
step scans for anything still holding data (e.g. an aborted
run where the phase-end handler didn't execute, or a component
that ingested outside the named boundaries above) and
force-closes it with the same blocking-fanout behavior.

Then `CadenceReporter::shutdown` drops every subscriber's
sender and joins the dispatch thread so each `Reporter::flush`
runs before the summary reads downstream sinks.

Each forced snapshot carries the same metadata as a
naturally-completed one (`captured_at`, `interval` reflecting
the actual elapsed accumulation time, not the nominal cadence
interval) so consuming systems can correctly interpret the
short trailing window. Consumers distinguish naturally-closed
from force-closed snapshots by the `interval` field: a
force-closed final snapshot has `interval < cadence`.

This guarantees no in-flight data is lost at end-of-run and
that end-of-run summaries reflect the entire session, not just
the last fully-completed window of each cadence.

---

## MetricsQuery — the unified read interface

All consumers (TUI, summary reports, SQLite emitter, GK
`metric()`/`metric_window()` nodes, programmatic callers) use a
single read interface. There is no per-consumer access layer; the
query interface speaks the metrics system's native types
(label sets, histograms, counters, gauges) and exposes three
uniform query modes.

The query holds two references: the component tree root (so it
can resolve component contexts and walk live instruments) and the
cadence reporter (so it can look up windowed snapshots). It is
the only path readers use — there is no separate "WindowedMetrics"
handle, summary-only path, or TUI-only path.

### Selection

Every query selects metric instances by one of:

- **Fully qualified label set** — exact match on a `Labels` value.
  Returns at most one instance per component context.
- **Label filter within a component subtree** — a predicate over
  labels, applied to instruments owned (transitively) by a named
  component. Matches zero or more instances. When the caller
  intends a specific metric, they assert exactly one match
  (otherwise a hard error — never silently pick the first).

Same selection grammar across all three query modes.

### Query modes

| Mode | Returns | Composition |
|---|---|---|
| `now` | Read-through to the live instrument(s). Histograms via [`Histogram::peek_snapshot`] (non-draining clone). | None — instantaneous reservoir peek. |
| `cadence_window(cadence)` | The last *full* coalesced window for the named cadence. Window age is an explicit property of the result so callers can decide whether it's fresh enough. | None — does NOT splice in `now`. |
| `recent_window(span)` | Approximation of "the last `span` of time": combines `now` with previously-completed cadence windows whose intervals tile `span`. | Per-metric ephemeral merge — only the matched metric instances combine, never the whole frame. |
| `session_lifetime` | The full canonical session span as of *now*. Combines the lifetime cadence's prebuffer with every smaller cadence's in-flight prebuffer plus a live `peek_snapshot`, so no in-flight data is missed. | Per-metric ephemeral merge — same cost rule as `recent_window`. See §"Canonical session-lifetime queries". |

`cadence_window` is the right call when the caller wants a
known-good fixed-window measurement (e.g. an alert that needs to
see "the last completed 1m window"). They may even poll for the
next snapshot to land before reading.

`recent_window` is the right call when the caller cares about
"approximately the last N seconds" and is willing to splice
fresh `now` data onto the most-recent full cadence windows.

### Combine semantics — algebraic uniformity

Snapshots with matching label sets are the same metric instance,
just at different points in time. Combining them is well-defined:

- **Histograms** combine reservoir-wise (`HdrHistogram::add`). All
  *distribution* properties (`p50`, `p99`, `min`, `max`, `mean`, and the
  per-window `count`) fall out of the combined reservoir — there is no
  per-percentile special case in the combine path. The separate monotonic
  `cumulative_count` is *carried*, not derived: it coalesces latest-wins
  up the cascade and sums on cross-component aggregate, exactly like
  `CounterValue::cumulative` (§"Histogram rank semantics", the
  cumulative-counter model).
- **Counters** sum.
- **Gauges** are last-write-wins by timestamp — the OpenMetrics/
  Prometheus/OTel gauge contract. A coalesced window's sample is the
  last-written scalar as of window end; any summarization (avg/min/
  max over time) is a query-point concern (`*_over_time` rollups),
  never a storage-side one. (Amended 2026-08-08: gauges previously
  weighted-averaged by interval, which stored values never written —
  a set-once count read 0.9994 — and silently redefined what every
  metricsql rollup meant against this store relative to every other
  PromQL-speaking system.)

Same rules wherever combining happens (cadence coalescing,
`recent_window` splicing, cross-component roll-up). A consumer
asking for `p99` is asking the combined reservoir for its 99th
percentile — that's an attribute of the result, not an input to
the combine.

### Cost rule for `recent_window`

`recent_window(span)` MUST NOT coalesce the full snapshot of every
metric just to extract one. The implementation walks the matched
selection only — combining the chosen metric's reservoirs across
the windows that tile `span`, plus a `peek_snapshot` from each
matched live instrument. Per-call cost scales with the number of
matched instances, not with total instrument count.

### Where the data lives

Windowed snapshots live inside the cadence reporter. On every
smallest-cadence tick the reporter walks the component tree once,
captures an OpenMetrics-compatible snapshot of every live
instrument (counter values, gauge values, histogram peeks), and
folds the results into its internal store keyed by
`(component_path, label_set, cadence)`. Coarser cadences are
populated by chained coalescing inside the reporter (SRD-42
§Tree Construction) — the reporter is the single writer.

The `MetricsQuery` interface is the single reader on top.

---

## Non-Draining "Now"

`Timer::snapshot()` has delta semantics — it *resets* the internal
histogram so consumers see only fresh samples. That's the right
behavior for per-interval reporters but wrong for `now()` which
multiple consumers may call between reporter ticks.

### `peek_snapshot` on Histogram

A parallel read-only API:

```rust
impl Histogram {
    /// Produce a snapshot by CLONING the current histogram
    /// rather than swapping it out. The instrument keeps
    /// accumulating from its existing state — no draining.
    pub fn peek_snapshot(&self) -> HdrHistogram<u64>;
}
```

Cost: one histogram clone. For 3-significant-digit HDR over a
1 h range that's ≈ 200 KiB — acceptable for occasional calls
(not per-sample).

The `now` query mode is implemented on top of `peek_snapshot` for
histograms and direct value reads for counters/gauges. Captured
results carry `captured_at = Instant::now()` and an `interval`
of zero (instantaneous read).

---

## Wire-Up

### The metrics cadence feed — the consumable service endpoint

The windowed-metrics service has **two intentionally separate
responsibilities**. They are an effective unit, but the boundary
between them is deliberate and must not be collapsed:

- **The metrics scheduler** (`nmbrs-metrics::scheduler`) is the
  *pacer*. A dedicated thread captures one OpenMetrics-shaped frame
  of the live component tree (`capture_tree`) at the base interval
  and ingests it into the cadence reporter. It owns *when* a frame
  is taken and *what* the tree currently holds.
- **The cadence reporter** (`nmbrs-metrics::cadence_reporter`) is the
  *publisher and store*. It is command-driven — it has **no internal
  timer**. That is deliberate, not passivity: being driven by
  ingest commands is what keeps it lock-free on the async hot path.
  On each ingested frame it runs the cascade (close → coalesce →
  promote, §"Streaming coalesce semantics"), **publishes** each
  closed window to subscribers, and retains it for pull reads.

Together they are the **metrics cadence pipeline**. Its consumable
endpoint — the canonical name used throughout the SRDs — is the
**metrics cadence feed**: the cadence reporter's per-cadence
publish/subscribe surface (`CadenceReporter::subscribe`), backstopped
by `MetricsQuery` for pull reads.

Canonical vocabulary (one name per concept — no synonyms):

| Term | Meaning |
|---|---|
| **metrics cadence feed** | the consumable service endpoint: subscribe to a cadence, receive its published windows |
| **subscribe to a cadence** | register a consumer on the feed for one declared cadence (`CadenceReporter::subscribe`) |
| **cadence pulse** | one interval closing and being published to subscribers |
| **published MetricSet** (cadence window) | the immutable `Arc<MetricSet>` delivered on a pulse |
| **cadence read** | reading the current framed window *without* subscribing, via `MetricsQuery` |

A consumer that needs to *react* to fresh data (the SQLite emitter
today; the settle/objective evaluation of SRD-86 §"Settling")
**subscribes to a cadence** on the feed and acts **per pulse**. A
consumer that only needs the latest value performs a **cadence read**.
Nothing outside the scheduler captures or re-feeds, and nothing on the
consumer side snapshots or frames — that responsibility lives only in
the pipeline.

### Cadence reporter — single writer

The cadence reporter is the single writer of windowed snapshots — but
it does not capture them. The metrics scheduler (above) paces the
capture: on every base-interval tick it walks the live component tree
once, captures an OpenMetrics-shaped frame of every instrument
(counter values, gauge values, non-draining histogram peeks), and
ingests that frame into the reporter. The reporter then, per ingested
frame:

1. Folds the frame into the smallest-cadence window's accumulator.
   When that accumulator's interval is satisfied, it closes into an
   immutable snapshot, publishes it to subscribers, and folds it into
   the next-larger cadence's prebuffer (see §"Streaming coalesce
   semantics" above for the full cascade).
2. Publishes each closed snapshot into the reporter's internal store
   keyed by `(component_path, label_set, cadence)`, replacing the
   previous "last closed window" for that key. Older windows are
   retained in the per-cadence ring (SRD-42 §Phase 4) for
   `recent_window` queries.

The reporter has no internal timer — the cascade runs only when a
frame is ingested; the scheduler supplies the timing.

There are no per-component windowed handles. Components own their
instruments; the reporter owns the windowed view of those
instruments over time.

### MetricsQuery — single reader

A single `MetricsQuery` instance is constructed at session start
with references to the component tree root and the cadence
reporter. Every consumer (TUI, summary reporter, SQLite emitter,
GK `metric()`/`metric_window()` nodes, programmatic callers) reads
through it.

### Subscription model — pull vs push

`MetricsQuery` is the *pull* surface. `cadence_window(cadence)`
returns the most-recently-closed snapshot for that cadence
immediately, even for a consumer that attached mid-session — it's
just a read of the reporter's store, no notification involved.

Push-style notification is a separate subscribe call: a consumer
that wants to react to *future* snapshot closures registers a
subscriber per cadence. Mid-session attachers get only future
closes; they read the most-recent past close via `cadence_window`
in their first call if they need that catch-up data.

Pull and push use the same `Arc<Snapshot>` instances — a
subscriber's first push and a same-cadence pull immediately
afterward see the same object.

### Reports — lifetime accumulator + opt-in cadences

The summary report defaults to the session-lifetime accumulator,
which is itself one of the cadences maintained by the reporter
(the largest declared cadence, conventionally treated as
"lifetime" when its interval ≥ session duration). Users specify
additional cadences as opt-in extra columns, named via the
declared-cadence list — no fixed `ActivityRow` schema; each row
is built by enumerating components and filling cells via
`MetricsQuery` calls against the configured cadences.

### TUI

Reads the same query interface. The bar chart enumerates declared
cadences via `cadence_window`, the "now" column via `now`, and an
ad-hoc lookback (when surfaced in the UI) via `recent_window`.

### Component lifecycle: `scope_close` flush

A phase whose duration is **shorter than the smallest cadence
interval** can finish between two cadence pulses, leaving every
sample emitted by its instruments unflushed when the component
drops at scenario teardown. Without intervention, those samples
never reach the cadence stream — the instrument was the only
thing holding them, and the next pulse fires after the
component is gone.

The runtime emits a **`scope_close`** event to the cadence
reporter at component teardown (phase end, scenario end,
op-dispenser drop). The event carries:

- The component's logical name + effective labels.
- A list of every instrument owned by the component.
- A "partial window" timestamp marker.

The reporter responds:

1. Pulls the current delta from each named instrument
   (without resetting future windows — the instrument is
   ending, but the stream isn't, and other components in the
   tree continue).
2. Records the partial window as a snapshot annotated
   `partial=true`.
3. Folds the partial sample into the next full cadence window
   when one closes. Aggregation per instrument kind:
   - **Counter** — sum of partials in the window.
   - **Gauge** — last `set()` value across partials in time
     order.
   - **Histogram** — HDR-style merge.

This is the same aggregation the streaming-coalesce semantics
(§"Streaming coalesce semantics" above) already perform within
a single window for normal cadence-pulse-flushed samples;
partial windows just carry the annotation so downstream tooling
can distinguish them if needed.

The flush is **generic** — every short component benefits, not
just SRD-40b synthetic-metric phases. Synthetic phases are the
sharpest test case (often very short, often with one cycle per
cell), but a bursty for_each iteration emitting two cycles per
sub-iteration also relies on this contract to avoid losing
samples to the cadence-window boundary.

After the flush, the component is free to drop. Its
instruments' data lives in the stream; future window
aggregations include the partial-annotated samples
transparently.

---

## Snapshot data model — OpenMetrics-aligned

Every snapshot the cadence reporter holds is shaped to mirror the
[OpenMetrics specification](https://github.com/prometheus/OpenMetrics)
1:1, using the spec's terminology unchanged. External consumers
(Prometheus scrape, OTel translation, third-party dashboards) get a
near-trivial projection because the in-memory model already speaks
their structure, types, and naming conventions.

### Container hierarchy

| Layer | OpenMetrics term | Contents |
|---|---|---|
| top-level snapshot | `MetricSet` (§4.1) | zero or more `MetricFamily`, names unique |
| family | `MetricFamily` (§4.4) | `name`, `type`, optional `unit`, optional `help`, list of `Metric` |
| series | `Metric` (§4.5) | `LabelSet` (unique within family), ordered list of `MetricPoint` |
| observation | `MetricPoint` (§4.6) | typed value variant + optional `timestamp` |

**Identity of a time series is `(MetricFamily.name, LabelSet)`**
per §4.5.1 — the same identity used for cascade-time combine
(matching identity → matching reservoir / counter / gauge → combine
allowed).

### MetricPoint variants

We mirror the spec's value variants verbatim:

- **`CounterValue`** (§5.1.1) — `total`, optional `created`,
  optional `exemplar`. Sample name carries the spec-required
  `_total` suffix on exposition.
- **`GaugeValue`** (§5.2.1) — `value`.
- **`HistogramValue`** (§5.3.1) — `count` (required), `sum`
  (required when no negative observations), optional `created`,
  cumulative `Bucket` list with final `upper_bound = +Inf`,
  per-`Bucket` optional `exemplar`.
- **`SummaryValue`** (§5.6.1) — `quantile` entries, optional
  `sum`/`count`/`created`.
- **`InfoValue`** (§5.4.1) — labelset only; implicit value 1.
- **`StateSetValue`** (§5.5.1) — set of `(name, bool)` states.
- **`UnknownValue`** (§5.7.1) — `value`, untyped fallback.
- **`GaugeHistogramValue`** (§5.8.1) — same shape as
  `HistogramValue` with `gsum`/`gcount`, per-bucket exemplars
  permitted.

Initial implementation covers `Counter`, `Gauge`, `Histogram` —
the others are added when a real consumer needs them.

### Exemplars

Exemplars (OpenMetrics §4.6.1) link a metric observation to an
external context — typically a trace/span ID, a workload cycle
number, or a sample identifier. They're attached to specific
points, propagate through the cascade, and project verbatim to
exposition.

#### Where exemplars attach

- **Counter** — at most one `exemplar` per `CounterValue`
  (`Option<Exemplar>` field).
- **Histogram / GaugeHistogram** — at most one `exemplar` per
  `Bucket` within the value's bucket list. A 20-bucket histogram
  may carry up to 20 exemplars per `MetricPoint`.
- **Other variants** — spec does not permit exemplars; we
  follow.

An `Exemplar` carries:

- a `LabelSet` (typically `trace_id`, `span_id`, but any
  spec-conformant label set is allowed; total serialized length
  ≤ 128 UTF-8 characters per spec §4.7),
- a `value` (the observation value the exemplar refers to —
  matches the metric's value type),
- an optional `timestamp` (when the observation occurred).

#### Recording side — sampling

Exemplars are **sampled, not exhaustive**. The instrument's
record path takes an optional exemplar argument; per-bucket
sampling discards new exemplars when the bucket already has one
for the *current open window* (cheap O(1) check on the
accumulator). Workloads pay the LabelSet-allocation cost only
when they actually attach an exemplar — recordings without an
exemplar argument are exemplar-free at zero overhead.

`Counter::increment_with_exemplar(value, exemplar)` and
`Histogram::record_with_exemplar(value, exemplar)` are the new
recording surfaces; the existing `increment` / `record` paths
stay untouched.

#### Combine semantics

When snapshots fold up the cascade (or are merged in
`recent_window` / `session_lifetime`), exemplars from matching
positions combine by **most-recent-wins**:

- Counter exemplar: the contributing snapshot with the latest
  `MetricPoint.timestamp` provides the combined exemplar.
- Histogram bucket exemplar: same rule, per bucket
  independently.

Most-recent-wins is the default because it gives operators a
trace-link they can probably still resolve in their tracing
backend (older traces are typically expired). A future opt-in
strategy could prefer extreme-value exemplars (the slowest
observation in a latency bucket) — captured as a follow-up, not
in the initial implementation.

#### Cost rule

Carrying exemplars across cascade folds costs an `Arc<LabelSet>`
clone per bucket per fold. For a 20-bucket histogram across a
6-layer cascade, worst case ≈ 120 LabelSet `Arc` clones per
smallest-cadence tick — negligible. Storage cost is one
`Option<Arc<Exemplar>>` per bucket per stored snapshot, additive
to the existing ring memory footprint.

### Histogram representation — internal vs exposition

The OpenMetrics on-wire representation of a histogram is
*cumulative buckets* — `(upper_bound, count)` with the final
bucket at `+Inf`. That format is lossy for combine: merging two
cumulative-bucket histograms requires the same bucket layout, and
we lose the ability to derive accurate quantiles after combine.

For internal storage we keep the **HDR reservoir** (`HdrHistogram<u64>`)
as the source of truth on the `HistogramValue` — that's what
combines correctly across cascade folds and `recent_window` /
`session_lifetime` ephemeral merges. The OpenMetrics-shaped
`Bucket` list is **derived on demand at exposition time** from the
HDR reservoir against the consumer-requested bucket layout.
`sum` and `count` are also derivable from the reservoir but are
maintained alongside it for O(1) access.

### Naming convention

Suffix rules from spec §4.4.1 / §5.x are enforced at exposition,
not at storage. Internally a counter is named `cycles` (no
`_total` suffix); the exposition layer appends `_total` per spec.
Likewise `_count`/`_sum`/`_bucket`/`_created`/`_info` are
exposition-time concerns. This keeps the in-memory model free of
suffix bookkeeping and lets the exposition layer enforce spec
compliance in one place.

### Timestamps

`MetricPoint.timestamp` (§4.6, optional in spec) is **always
populated** in our snapshots — the cadence-window-close instant
for cadence-window points, the live read instant for `now`
points, the merge instant for `recent_window` /
`session_lifetime` ephemeral points. There is no per-`Metric` or
per-`MetricSet` timestamp; consumers read it off the points as
the spec requires.

`_created` (Counter/Histogram/Summary) carries the metric's
series start time — for our use, the moment the owning component
started running. This lets external consumers detect counter
resets per spec §5.1.

### Component identity as labels

A metric's owning component context is encoded as labels on its
`LabelSet`, not as a structural layer outside the OpenMetrics
shape. Conventional label names (subject to refinement in Phase 7
when the convention is exercised by `MetricsQuery`):

- `component` — the component path, e.g.
  `session.run-2026-04-22.phase-load`.
- Standard nmbrs activity/phase labels carry through unchanged.

Subtree-filter selection in `MetricsQuery` walks the `LabelSet`s,
optionally backed by an auxiliary component-path index in the
store for O(1) subtree lookups when matching is hot.

---

## SQLite — near-time persistence, not a query surface

The SQLite reporter is a persistence sink, not the canonical
query interface. It writes at one of the declared cadences, never
at the base interval:

- Default minimum write cadence is `10s`. Per-second writes
  saturate disk for no operator value.
- If `10s` is in the declared cadence list, use it.
- Otherwise, use the next-higher declared cadence above `10s`.
- If no declared cadence is ≥ `10s`, use the largest declared
  cadence (best effort).

Schema design is downstream of the `MetricsQuery` interface, not
the other way around — operators query the in-memory interface
for live decisions and use SQLite for after-the-fact analysis or
external tooling.

---

## Compatibility

The scheduler's per-reporter-interval API stays intact.
`MetricsQuery` and the cadence reporter's internal windowed store
are an additive read/write pair on top of the same scheduler tree.
Existing reporters continue receiving their coalesced frames at
the cadences they register.

---

## Design decisions

- **Histogram retention for `past()` queries.** Each cadence
  keeps a fixed ring of `HISTORY_RING_CAP=32` coalesced
  histograms — bounded memory regardless of run length
  (`cadences × 32 × ~200 KiB`). `past(span)` walks the smallest
  cadence whose ring fully covers `span`, falling through to
  larger cadences when finer rings haven't accumulated enough.
- **Cross-component queries.** The cadence reporter's snapshot
  capture walks the component tree at every smallest-cadence
  tick, so cross-component visibility is intrinsic — there is
  no separate per-component windowed handle to compose. A
  session-level query is just a `MetricsQuery` call with the
  session's component as the subtree root.
- **Default max fan-in (`K=20`).** With streaming coalesce, `K`
  barely affects per-tick cost or steady-state memory — what it
  actually controls is (a) in-flight data loss on crash (one
  prebuffer per layer, worst-case `K × layers × base_interval`
  of unmaterialized snapshots — ≈2 min at K=20 / 6 layers / 1s
  base), and (b) `recent_window` / `past(span)` granularity
  (lower K inserts more hidden layers, giving these queries
  finer tiles to combine). Operators tuning it should ask "how
  granular do I want my windowed-history queries?", not "how
  fast do I want coalesce to run?" — the latter is invariant in
  K.
- **Lifetime as a cadence.** Falls out of the streaming
  cascade: a cadence whose interval ≥ session duration never
  promotes mid-session, so its prebuffer simply accumulates
  every smaller-cadence close indefinitely.
  `cadence_window(LIFETIME)` peeks that prebuffer rather than
  waiting for a promotion. No separate accumulator API needed —
  see §"Streaming coalesce semantics → Why this matters →
  Lifetime is a natural emergent layer".
- **Cadence validation timing.** Cadence validation is a
  plan-time hard error (cheap; runs only on tree mutation).
  `Cadences::new`'s 1s minimum becomes a planning-time invariant
  parameterized on the actual base interval, not a parse-time
  constant. Removes the test-only `from_unchecked` escape hatch.

---

## Incremental Rollout

### Bootstrap phases (shipped — single-metric, per-component layer)

These phases shipped `WindowedMetrics` as a per-component access
layer for the latency timer specifically. They proved the
cadence-tree, auto-intermediate, peek_snapshot, and `past(span)`
mechanics on a real consumer (TUI). The unified
`MetricsQuery` design (above) supersedes the per-component layer
but reuses every mechanism below.

1. **Phase 1 ✓** — `Cadences` parsing / validation, basic
   `WindowedMetrics` per-component layer with `ingest` / `window`
   / `all_windows` / `lifetime` read API.
2. **Phase 2 ✓** — `CadenceTree::plan(declared, max_fan_in)`
   synthesizes hidden layers, rounds to nice durations, emits
   declared / inserted / realized-tree summary at INFO. Scheduler
   chained-tree consumption via
   `SchedulerBuilder::with_cadence_tree(...)`.
3. **Phase 3 ✓** — `Histogram::peek_snapshot` and matching
   `Timer::peek_snapshot`. The per-component `now()` reads via
   `set_live_source` callback; falls back to smallest-cadence
   window when unregistered.
4. **Phase 4 ✓** — `past(span)` per-component. Bounded
   `HISTORY_RING_CAP=32` ring per cadence; merges
   `ceil(span / cadence)` recent buckets.
5. **Phase 5 ✓ (TUI only)** — TUI bar chart reads cadences +
   per-window percentiles + live "now" via the per-component
   layer.

### Unification phases (planned — direct replacement, no compat layer)

This is a greenfield codebase: the bootstrap phases shipped to
prove the cadence/peek/past mechanics on a real consumer (TUI),
not to establish a public API contract. The unification phases
**replace** the bootstrap structures wholesale rather than
shimming them. There is no compat facade, no parallel write
path, no deprecated types kept alive for callers we don't have.

Each phase is a single change set: every caller of the
type/path being deleted gets rewritten in the same diff.

6. **Phase 6 — Snapshot data model is the only data model.**
   `crates/nmbrs-metrics/src/snapshot.rs` (the OpenMetrics-shaped
   `MetricSet`/`MetricFamily`/`Metric`/`MetricPoint` types) is
   the single in-memory representation of metric values. The
   `Sample` enum in `frame.rs` and its consumers are deleted
   and rewritten to produce/consume `MetricPoint` variants
   directly. `MetricsFrame` is replaced by `MetricSet` at the
   capture and coalesce paths.
7. **Phase 7 — Cadence reporter is the only store; `MetricsQuery`
   is the only reader.** The cadence reporter owns the
   `(component_path, label_set, cadence) → MetricSet` store and
   is the sole writer of windowed snapshots. `WindowedMetrics`,
   `InProcessMetricsStore`, and per-component snapshot allocation
   are deleted. `MetricsQuery` (`now` / `cadence_window` /
   `recent_window` / `session_lifetime`) is the only read path;
   every consumer (TUI, summary, SQLite emitter, GK
   `metric()`/`metric_window()` nodes) is rewritten against it
   in the same diff.
8. **Phase 8 — Streaming coalesce is the only coalesce.** The
   close-then-promote cascade described in §"Streaming coalesce
   semantics" replaces the current per-tick scheduler coalescing.
   Async notification dispatch with timeout escalation goes in
   here. `set_live_source` closure plumbing is deleted (the
   reporter owns the live capture path directly).
9. **Phase 9 — Cleanup invariants.** Replace the `Cadences`
   parse-time 1s minimum with a plan-time check parameterized
   on the actual base interval; delete `from_unchecked`. Surface
   the TUI `past(span)` overlay against `MetricsQuery::recent_window`.
   Verify no orphaned types or transition wiring remain.
