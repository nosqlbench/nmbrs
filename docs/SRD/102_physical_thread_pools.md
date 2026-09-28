# SRD-102 — Named Physical Thread Pools

**Status:** IMPLEMENTED (2026-07-06)

## 1. Problem

Realtime-sensitive, schedule-keeping work — the metrics cadence scheduler
(`crates/nmbrs-metrics/src/scheduler.rs`), and future low-jitter dispatchers — currently
runs as `tokio::spawn` tasks on the **shared async runtime** alongside the
workload fibers. Under load the timer fires on time but the task is polled late
(the runtime is busy running fibers), producing tick drift (observed
`actual=1.054s` vs `nominal=1s`, 5.5% off). Tokio has **no task priority**: a
co-resident task cannot be guaranteed CPU ahead of the fibers. The only robust
lever is *isolation* — a real OS thread that does not share duty with the async
worker pool.

We generalize this: a process-wide facility to allocate **named physical thread
pools** — sets of dedicated OS threads with an explicit scheduling policy — so
realtime-sensitive roles never share a thread with the workload runtime, and so
the allocation is a system-wide configurable with reasonable defaults.

## 2. The abstraction

A **physical thread pool** is a fixed set of real OS threads (`std::thread`, not
tokio tasks) dedicated to exactly one named role, each carrying a scheduling
policy (priority + optional CPU affinity). "Physical" + **no double duty**: a
thread belongs to one pool; realtime work never shares a thread with the async
worker pool or unrelated work.

A process-wide `ThreadPools` registry owns the pools and hands out spawn
handles. Consumers request by name:

```rust
pools.timing().spawn("cadence", move |stop| { /* periodic loop */ });
```

The registry is constructed once at startup from resolved config and lives for
the process. `workers` is the tokio async runtime; `timing`/`io` are std-thread
pools.

## 3. Pools + defaults

| Pool      | Role                                             | Default size        | Policy |
|-----------|--------------------------------------------------|---------------------|--------|
| `timing`  | Low-jitter periodic dispatch (cadence scheduler, future realtime timers) | 1 thread | attempt `SCHED_RR` → `nice` → plain (logged); pin to a reserved core |
| `workers` | Tokio async runtime (fibers / workload)          | `cores − reserved`  | normal; capped so dedicated pools always have a core |
| `io`      | Offloaded reporter / report I/O                  | 1–2 threads         | normal priority |

`reserved = timing.threads + io.pinned` cores; `workers` is sized to the
remainder so the `timing` thread is never queued behind a fiber. On very small
core counts (< 3) the reservation degrades gracefully (pools may share the
default policy; logged).

## 4. Config surface (system-wide)

`ThreadPoolConfig` — per pool `{ threads, sched_policy, affinity }` — resolved
from CLI flags and env with core-count-derived defaults:

```text
--threads.timing=1              # thread count per pool
--threads.workers=N
--threads.io=2
--threads.timing.sched=rr|fifo|nice|none   # scheduling class (default rr)
--threads.timing.pin=<core|auto|off>       # affinity (default auto = reserve a core)
```

Env mirrors: `NMBRS_THREADS_TIMING`, `NMBRS_THREADS_TIMING_SCHED`, etc. A single
resolution point produces the immutable `ThreadPoolConfig`; the registry applies
it. Unknown pool names / policies are hard errors at startup (never silently
ignored — cf. `max_batch_size`, docs/TODO.md).

## 5. Scheduling policy (assertive, graceful, logged)

Applied at thread spawn via `libc` (already linked), best-effort with graceful
fallback and a one-line startup log of what was actually achieved:

- `timing` **attempts `SCHED_RR` (realtime) by default.** If the process lacks
  `CAP_SYS_NICE`, fall back to a `nice` bump (`setpriority`), then to plain
  scheduling — each transition logged so a low-privilege environment is visible,
  not silent. `SCHED_RR` (not `FIFO`) so a misbehaving thread can't hard-starve
  a core.
- Affinity via `sched_setaffinity`; `pin=auto` reserves a core for the `timing`
  pool and caps `workers` to the rest.
- Non-Linux: policy application is a logged no-op; the isolation (dedicated
  thread) still holds.

Rationale: for a mostly-sleeping periodic tick, dedicated-thread isolation alone
brings jitter well under 1% (OS timer slack ≪ tick). Realtime priority + pinning
is the assertive belt-and-suspenders for a fully-saturated box.

## 6. First consumer — the cadence scheduler

`Scheduler::start` (`crates/nmbrs-metrics/src/scheduler.rs`) moves off the shared runtime
onto a `timing`-pool thread:

- **Isolation (#1):** the tick loop runs on `timing`; the tokio `Notify`/`done`
  shutdown wiring becomes a std stop signal (`Condvar` / `recv_timeout`) the loop
  waits on to the absolute `next_tick` (fixed-rate preserved).
- **Minimal hot path (#3):** the tick thread only *captures* per-component deltas
  and hands them to the `io` pool via a channel; `report()` (potential I/O) runs
  on `io`. The timing thread's critical section is capture + enqueue.
- **Dual timestamps + divergence warning (#2 variant):** each `MetricSet`
  snapshot carries **both** `scheduled_ts` (the nominal `next_tick`) and
  `actual_ts` (`Instant::now()` at fire). Logical/cadence processing (coalescing,
  windowing, rate dt) keeps using `scheduled_ts` unchanged — the prescribed
  cadence stays canonical as a matter of record. If
  `|scheduled_ts − actual_ts| > 250 ms`, emit a warning to the standard warning
  channel (`crate::diag::warn`). This replaces the current 5%-consecutive-interval
  heuristic with an absolute schedule-vs-reality check.

## 7. Decisions (resolved 2026-07-06)

1. **Config surface:** CLI flags + env, core-count-derived defaults. (Not a
   separate config file for v1.)
2. **Realtime default:** `timing` attempts `SCHED_RR` by default, degrading to
   `nice` → plain with a startup log. Never errors on an unprivileged box.
3. **Core reservation:** `pin=auto` reserves + pins a core for `timing` and caps
   `workers` by default.

## 8. Non-goals / future

- Not a general work-stealing scheduler; pools are role-fixed.
- No per-task priority within a pool.
- Dynamic resize at runtime is out of scope (config is start-time immutable).
- Future realtime consumers (servo control loops, deadline dispatchers) attach to
  `timing` (or a new named pool) without touching the async runtime.
