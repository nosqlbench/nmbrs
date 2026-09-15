# SRD-88: Concurrent In-Process Executions

> **Status:** DRAFT — model agreed (2026-06-20). Extends
> [SRD-77](77_working_sessions_and_refine.md)'s Session/Execution
> entity model with a **concurrency** dimension: multiple executions
> running *at the same time*, in *one process*, all sharing *one
> session*. SRD-77 models executions temporally (sequential
> `run`/`resume`/`refine` invocations sharing a session over time); this
> SRD makes them concurrent. The load-bearing artifact is a **task-local
> `ExecutionContext`** that replaces the process-global "one run per
> process" singletons.

## 1. Ownership & relationships

Owns **how N executions coexist in one process without colliding**. Sits
across:

- [SRD-77 working sessions](77_working_sessions_and_refine.md) — owns the
  `Session ⊃ Execution[]` entity model + `exec_id`. This SRD adds
  concurrency to it; `exec_id` is the partition key.
- [SRD-45 sessions](45_sessions.md) — owns the session = one directory /
  `metrics.db` / `session.log` / `checkpoint.jsonl`. Those stay **one per
  session**, now written by N concurrent executions, `exec_id`-tagged.
- [SRD-87 output channel](87_output_channel.md) + [SRD-41](41_logging.md)
  — the observer + output channel become **per-execution** (task-local)
  instead of process-global.
- [SRD-02 concurrency](02_concurrency_model.md) — the per-execution
  context rides the existing `tokio::task_local!` fiber-context
  machinery; no new threading model.

The forcing question: **can a deeply-nested fiber find *its* execution's
observer / channel / scene-tree / stop-flag without a process-global —
so two executions in one process route their output, state, and control
independently while sharing the durable session store?** This SRD says
yes, via a task-local `ExecutionContext`.

## 2. The two tiers

```
 SESSION  (one directory; shared, exec_id-tagged, concurrent-safe writers)
   ├─ metrics.db        rows tagged exec_id          (already, SRD-77)
   ├─ session.log       lines tagged exec_id         (decision: one file)
   ├─ checkpoint.jsonl  events tagged exec_id         (decision: one file)
   ├─ SESSION POLYDAT SCOPE  ← the common root (captures the process args:
   │                            workload params + session-level CLI). One per
   │                            process. Every execution DERIVES its polydat
   │                            tree from this scope (GK scope composition).
   └─ EXECUTIONS[]   1..N, CONCURRENT, each task-local:
        ├─ exec_id              (allocated under one global Mutex)
        ├─ observer             (display/lifecycle routing)
        ├─ output channel       (SRD-87 buckets)
        ├─ scene tree           (its own scenario tree + status)
        ├─ derived polydat sub-scope (its scenario tree, composed UNDER the
        │                            shared session scope — bind_outer_scope)
        ├─ run-state            (snapshot/actor, when displayed)
        └─ stop flag            (Ctrl-C / stop scoped to one execution)
```

**Shared (per-session):** the directory, the three durable stores, **and the
session polydat scope** — a GK scope that captures the process arguments
(workload params, session-level CLI) and is the **common root** every
execution derives its polydat tree from (SRD-13c/67 scope composition). The
session component root (`runtime_context::SESSION_ROOT`) is this shared root,
NOT a per-execution thing: each execution's components hang under it,
distinguished by `exec_id`.

**Isolated (per-execution, task-local):** observer, output channel,
scene tree, the **derived** polydat sub-scope (composed under the shared
session scope), run-state, stop flag, `exec_id`.

The session-scope-as-common-root is the load-bearing refinement: rather than N
independent roots colliding, there is ONE session scope (the process's
argument capture) and N executions branching from it — the GK canonical-scope
model applied to the session/execution split. `session_root` therefore stays
shared (a per-session global is correct for one process / one session); the
work is the *derivation* (each execution's scenario scope binds the session
scope as its outer), not de-globalizing the root.

## 3. The load-bearing seam — task-local `ExecutionContext`

Today the isolated-tier items are **process-global** statics
(`GLOBAL_OBSERVER`, `CHANNEL`, `GLOBAL_TREE`, `SESSION_ROOT`,
`SESSION_STOP`, the log-level `OnceLock`s — full inventory in §7). A
deeply-nested fiber reads them via free functions (`global_observer()`,
`output_channel::installed()`, `scene_tree::current()`,
`session_root_handle()`, `stop_requested()`).

The fiber context is **already** `tokio::task_local!` (SRD-02). So:

```rust
pub struct ExecutionContext {
    pub exec_id: u64,
    pub observer: Arc<dyn RunObserver>,
    pub channel: Arc<dyn OutputChannel>,
    pub scene_tree: Arc<RwLock<SceneTree>>,
    pub session_root: Arc<RwLock<Component>>,
    pub stop: Arc<AtomicBool>,
    pub display_level: LogLevel,
    pub retain_level: LogLevel,
    // session-shared services (Arc clones, not owned):
    pub session: Arc<Session>,
}

tokio::task_local! {
    static EXEC_CTX: Arc<ExecutionContext>;
}
```

An execution runs its scenario tree inside `EXEC_CTX.scope(ctx, async {
… })`. Every former global *read* becomes **task-local-first with a
process-default fallback**:

```rust
pub fn current_observer() -> Arc<dyn RunObserver> {
    EXEC_CTX.try_with(|c| c.observer.clone())
        .unwrap_or_else(|_| process_default_observer())
}
```

**Axiom A1 — additive de-globalization.** The override is *additive*:
the existing process-globals stay and remain the **process default**.
Code that does not run inside an `EXEC_CTX.scope` (bootstrap, the CLI,
single-run `nmbrs run`, tests) reads the default — **behavior is
byte-identical to today**. Only concurrent executions set the task-local
and get isolation. This is what makes the migration safe and
incremental: every accessor change is a no-op until someone scopes a
context.

**Axiom A2 — `exec_id` is the only new global.** Allocating `exec_id`
needs one process-global counter under a `Mutex`/`AtomicU64` so two
concurrent fresh executions can't both claim `1`. That single
synchronization point is the entire unavoidable global surface.

**Axiom A3 — session services are shared and `exec_id`-tagging.** The
log sink, checkpoint writer, and sqlite reporter are *session*-scoped
(one per session), concurrent-safe, and stamp every line/event/row with
the writing execution's `exec_id` (read from the task-local context).
They are NOT per-execution.

## 3c. Encapsulation & lock-freedom (invariant)

Two executions sharing a session **must not synchronize on each other**,
and **no shared mutable state in the session layer may be mutated by a
workload layer**. Concretely:

- **Per-execution mutable state lives in the `ExecutionContext`** (task-
  local), never a process-global: `exec_id`, stop flag, observer, scene
  tree, output channel. A workload mutates only *its own* context — a
  neighbour's is a different `Arc`. The process-globals remain only as
  the A1 single-run fallback (no scoped context ⇒ no concurrency).
- **Session services are lock-free on the workload hot path.** Metrics
  flow through the `CadenceReporter` (actor + `ArcSwap` + lock-free
  channel — [[feedback_lock_free_metrics]]); op output / log / status
  through the `ArcSwapOption` channel; `exec_id` allocation and stop are
  atomics. Phase-end trigger dispatch (`phase_end_triggers::fire`) takes
  a lock-free atomic-guarded fast path (`TRIGGER_COUNT`) so the common
  no-trigger case — every concurrent-workload session today — never
  contends on the global registry lock.
- **Live metric reads are `exec_id`-scoped (encapsulation).** The
  in-process metric store (`queryapi::live_access`) is shared and holds
  every execution's series; a workload reading it (an optimizer's
  `metricsql_scalar("sum(rate(errors_total[…]))")`) must see only *its
  own* series, else a concurrent neighbour writing the same metric name
  skews the result — the optimizer converges to the wrong coordinate.
  The series already carry the writer's `exec_id` (component scope tag);
  `MetricsQueryAccess::select_range` now drops any series whose `exec_id`
  isn't the reading execution's. The reader learns *which* execution is
  asking via `queryapi::install_read_exec_id_hook` — `exec_id` lives in
  nmbrs-runtime's task-local `ExecutionContext`, a layer above
  nmbrs-metrics, so the runtime installs a one-line resolver hook rather
  than the lower crate reaching up. `None` outside any scope ⇒ unscoped
  (single-run, A1). This was a real concurrency defect: `control.yaml`'s
  optimizer intermittently converged to the wrong control value when
  other `errors_total`-emitting optimizer examples ran alongside it.
  **Superseded by SRD-89:** this post-filter becomes a by-default
  **matcher injection** (`session` + `exec_id`) at the query boundary,
  sourced from the execution component, so aggregations compute over only
  the scoped series and single-run is qualified identically (not a `None`
  special case).
- **The durable sqlite store is serialized, not contended.** It is the
  one place writes must serialize (sqlite is single-writer). The metric
  hot path is already non-blocking (cadence subscription); the remaining
  direct `sqlite_reporter.lock()` sites are per-*phase-end* / per-
  *execution-end* (report summaries, metadata, the executions-row close)
  — infrequent, brief, `exec_id`-tagged. **Open follow-up:** route these
  through the same non-blocking writer the metric path uses so an
  execution never *blocks* on a neighbour's flush, only the writer
  serializes — strict lock-freedom for the store. Design-first before
  implementing.
- **`runtime_context::SESSION_ROOT`** (the component root `control(...)`
  reads resolve against) is *session*-set once at `Session::new`, then
  workload-*read* only — it is not workload-mutated. Control resolution
  across concurrent executions with **same-named** controls would
  resolve against the shared root; isolating it (per-execution root in
  the context) is deferred — the up-walk + subtree-scope semantics make
  it a design-first change, not a mechanical one. **Specified in SRD-89:**
  control resolution roots at the execution component (the same
  "execution component is the dimensional root" rule as the metric
  queries above), so concurrent same-named controls — a servo retargeting
  `concurrency`/`rate` — become per-execution. This is the deterministic
  cross-talk behind the SRD-89 servo-example failures.
- **The metrics subsystem holds *zero dedicated threads* — it runs on
  the shared runtime.** `nmbrs-metrics` carries the tokio `rt` feature.
  The cadence reporter's single-writer **owner** (the lock-free actor
  draining the command stream) and the **scheduler** (the cadence-tick
  capture loop) are both `tokio::spawn`ed tasks, not `std::thread`s; the
  scheduler ticks on `tokio::time::interval` and stops via a `Notify`,
  signalling a `done` channel after its final flush so the `stop()` path
  can still guarantee the trailing window committed. The session-end
  lifecycle synchronizations **`quiesce` and `shutdown_flush` are `async`
  and `.await` the owner** (via an `Async`-variant `FlushAck` oneshot) —
  so they work on a **current-thread** runtime, where `block_in_place`
  would panic and a blocking wait would deadlock against the very owner
  task they await (2026-06-22). The remaining sync waits — the test-only
  `flush_for_tests` barrier and the scheduler's `Drop`-time `wait_for_done`
  — use `block_compensated` / a runtime-flavor-gated wait (multi-thread
  `block_in_place`; current-thread best-effort `try_recv`; no-runtime
  blocking `recv`).
- **Workload computations driven by a session service run *in the
  workload's context*, not on a bare service thread.** The signaling
  layer is **threadless**: no parked worker thread per subscriber. When a
  window closes, the owner task spawns an **ephemeral delivery fiber**
  per subscriber on the ambient runtime. Each fiber is wrapped in the
  subscription's `context_wrap` — a `Fn(Future) -> Future` supplied at
  subscribe time that applies `execution_context::scope` — so an
  optimizer's settle subscriber (`PhaseStopEvaluator`) pulls its
  `metricsql_*` objective as the **owning execution** (the read scopes to
  its own `exec_id`). Session-level subscribers (sqlite, per-instance)
  carry no wrap and run bare. Delivery is **serialized, lossless**
  (`reporter.lock().await`) so a durable sink never drops a window;
  backpressure is bounded at the fanout (`pending() >= channel_capacity`
  ⇒ drop + stall timeout). Likewise any fire-and-forget workload task
  (`control_set`'s async write, the per-cycle fibers) is `propagate`d so
  it carries the context. The invariant: **no workload task is ever
  spawned/driven attached to a context that isn't its own** — and the
  signaling that wakes it costs an ephemeral fiber, not a parked thread.

### Load-sensitive examples (testkit tuning)

A *causal* optimizer example (`optimizer/{control,saturation,hybrid,
metricsql,multiservo}.yaml`) settles a windowed objective read from the
live cadence feed — its convergence depends on wall-clock timing. Run
ten-at-once, CPU contention perturbs that timing. With reads now
correctly `exec_id`-scoped (each sees only its own backend's overloads),
the examples are made robust by **lowering their simulated intensity
without changing the signal**: the testkit overload threshold is set by
`rate × result-latency` (≈ in-flight depth), so quartering `rate` and
quadrupling `result-latency` holds the saturating depth — high
concurrency still overloads, `conc=2` still doesn't — at 4× lower op
throughput. Less CPU per example ⇒ less contention ⇒ the settle window
stays representative. (`rate`-as-search-axis examples scale the searched
rates too, preserving every `rate×latency` product.)

## 4. Headless-first (decision)

Phase 1 ships concurrent executions with **no live display**: each runs
headless, writes its `exec_id`-tagged data to the shared session store,
and returns its outcome. The library entry:

```rust
/// Run each spec as its own execution, concurrently, in one process,
/// sharing `session`. Headless — no live display surface. Returns each
/// execution's outcome in input order.
pub async fn run_executions_concurrent(
    session: Arc<Session>,
    specs: Vec<ExecutionSpec>,
) -> Vec<ExecutionOutcome>;
```

This unblocks the motivating use case (running many workloads in one
process — e.g. the example walker, programmatic batch runs) without the
display de-globalization, which is the hardest part and deferred to
Phase 3.

## 5. Durable stores — one file, `exec_id`-tagged (decision)

`session.log` and `checkpoint.jsonl` stay **single per session**; every
line/event carries its `exec_id` (matching `metrics.db`'s existing
model and SRD-77's "one append-only log with exec_id tags" lean). The
writers become concurrent-safe for in-process multi-writer (Phase 2):
the checkpoint flock + the log-sink channel already serialize writes;
the work is stamping `exec_id` (from the task-local) onto each record
and confirming the single-process multi-writer path is race-free.

## 5b. Report scope — workload-declared reports are `exec_id`-scoped

A `report:` (or `summary:`) section declared **in a workload** belongs
to the execution that declared it: its data query is narrowed to that
execution's `exec_id`, never spanning every execution that shares the
session. This is the report-layer corollary of "one store, `exec_id`-
tagged" — a session-level rollup (e.g. `session_summary`) reads
session-scope totals across all executions, but a workload's own report
reads only its execution's rows.

This matters the moment a session holds more than one execution — a
refine sequence (run → refine) or SRD-88 concurrent executions — where
an un-scoped query would aggregate unrelated runs.

- **Tables / summaries** already honor this: the in-run summary passes
  `Some(exec_id)` to its `ReportConfig` (`runner::report_config_from_summary`).
- **Plots** honor it via the persisted def: each `report.<name>` row is
  written under its declaring `exec_id` (`set_execution_metadata`), and
  the post-run plot renderer (`run::auto_render_plots`) injects
  `executions: <exec_id>` into each plot's spec —
  `latest_execution_with_metadata_like` surfaces that id. An author who
  pins an explicit `executions:` selection (`all` / `latest` / `<id>`)
  is honored, never overridden (`Never Ignore Silently`).

Open follow-up: the concurrent path (`run_executions`) renders no plots
yet — plot rendering lives cross-crate in `nmbrs` (post-run,
single-execution). Per-execution concurrent plot rendering needs a
registered render hook the workload-end report block can invoke with the
`exec_id`, the way tables already render in-runtime.

## 6. Pushes (sequenced)

1. **Task-local `ExecutionContext` + accessor override + `exec_id`
   allocator + headless concurrent entry.** De-globalize the reads
   (task-local-first, process-default fallback — A1), add the allocator
   (A2), and `run_executions_concurrent` (headless). Test: two+
   executions concurrently in one process get distinct `exec_id`s,
   independent stop flags, and return correct outcomes; the single-run
   path is unchanged. (First shippable slice.)
2. **Shared-store `exec_id` tagging + concurrent-writer safety.** Log
   sink / checkpoint / sqlite stamp `exec_id` from the task-local and
   are proven race-free for in-process concurrent writers.
3. **Concurrent live display.** A combined surface (or per-execution
   surfaces) for N concurrent executions — the SRD-87 channel +
   run-state actor go multi-execution. The hard UX part; deferred.
4. **Consumers.** Re-point the example walker (`verify_path`) and any
   batch path at `run_executions` for in-process concurrency instead of
   subprocess fan-out where it pays. **SHIPPED** for the example walker:
   `nmbrs/tests/example_workloads_in_process.rs` checks the whole
   `nmbrs/examples/workloads` tree as concurrent in-process executions sharing
   one session (≤10), with the **same** `#@`/`verify:` rules and
   `check_case_output` checker the `nmbrs check` CLI uses. Each case
   captures op stdout via its own `CaptureChannel`. This **retired** the
   subprocess-per-case walker (`nmbrs/tests/workloads.rs`); the `nmbrs
   check` CLI still drives the subprocess `verify_target`/`run_case` path
   (`nmbrs/tests/check_cli.rs` is its smoke test).

   **Load-bearing finding — the rule-matched phase count must come from
   the `RunState`, not the observer callbacks.** A dynamic loop
   (`do_until`/`do_while`) re-invokes one phase node, so the executor
   fires `phase_completed` once per *iteration* (a `do_until` body that
   runs 3× → 3 callbacks), the runtime scene tree keeps the *structural*
   node (1), and `phase_outcomes` records the last outcome (1) — three
   different counts. The post-run `session_summary` the example rules
   were written against (`#@ expect 2 completed`) counts a fourth thing:
   `RunState.phases` (`kind==Phase`, by status), which the TUI
   `run_state_actor` builds with find-pending-or-append semantics. The
   in-process walker therefore feeds each execution's lifecycle through a
   real `run_state_actor` (via `scenario_pre_mapped`→`InstallTree` +
   phase events) and synthesises its `phases:  C completed, F failed …`
   rollup from that RunState tally — so counts agree with the subprocess
   summary by construction. The shared `labeled_phase_rollup`
   (`readouts::builtins::session_summary`) is the one formatter for that
   line.

## 7. The de-globalization inventory (Push 1 surface)

| Global (today) | File | Becomes |
|---|---|---|
| `GLOBAL_OBSERVER` | observer.rs | `ctx.observer`, default fallback |
| `RETAIN_LEVEL` / `DISPLAY_LEVEL` | observer.rs | `ctx.{retain,display}_level`, default |
| `CHANNEL` | output_channel.rs | `ctx.channel`, default |
| `GLOBAL_TREE` | scene_tree.rs | `ctx.scene_tree`, default — **LANDED** |
| `SESSION_ROOT` | runtime_context.rs | **stays SHARED** — the session polydat scope / common root; executions *derive* (§2), not de-globalize. **LANDED:** the session component now carries `session=<id>` ONLY; the `exec_id` + `workload` labels moved to a per-execution **Execution component** child (`Execution::start`, `session.rs`), composed under the shared session root. Phase components attach under the execution component, so each execution's metrics carry its own `exec_id` without any tier redeclaring a label (SRD-19 label-ownership invariant). |
| `SESSION_STOP` / `GRACEFUL_STOP` | session_signals.rs | `ctx.stop`, default — **LANDED** |
| `GLOBAL_OBSERVER` / levels | observer.rs | `ctx.observer`, default — observer **LANDED**; levels pending |
| `GLOBAL_LOG_SINK` | log_sink.rs | **stays session-shared** (A3); `exec_id`-tags |
| `exec_id` allocation | (new) | one process `AtomicU64` (A2) — **LANDED** |

**Push 1 status (2026-06-20):** `execution_context.rs` (task-local
`ExecutionContext` + `exec_id` allocator), the stop-flag + observer +
scene-tree de-globalizations (additive, A1-verified), and the headless
`concurrent.rs` harness (`run_executions_concurrent` + `HeadlessObserver`)
have **landed and are tested** (isolated stop / observer routing concurrently;
single-run byte-identical).

**Component-tier split LANDED (2026-06-20):** the `Session` struct now
holds session-tier state only (`id`, `output_dir`, `component`,
`metrics_query`); the per-execution identity (`exec_id`, `workload`,
`scenario`, `verb`, `started_at`) and its component moved to the
`Execution` tier (`Execution::start` derives the execution component as a
child of `session.component`). The session component carries `session=<id>`
ONLY; the execution component carries `{exec_id, workload}`; phases attach
under the execution component. This is the metrics-separability prerequisite
for concurrent executions (each execution's metric rows carry its own
`exec_id`) and it is byte-identical for single-run — the leaf metric's label
*set* is unchanged (SRD-19 label-ownership invariant). `Session::resume` +
`Session::refine` collapsed into one `Session::reattach`; the verb +
`exec_id` are chosen by the runner and passed to `Execution::start`.

**Remaining for a concurrent *workload* run:** the run-path factoring
(§9 — `SessionHost::setup` + `run_execution`), output-channel + log-level
de-globalization (same additive pattern); the per-cycle fibers already carry
the context via `execution_context::propagate` at the two `JoinSet::spawn`
sites.

Already per-execution (no change needed): `Session`/`Execution`,
`ExecCtx`, `CheckpointWriter` (one per session — A3), the component
tree, activities, fiber task-locals, metrics instruments.

## 8. Load-bearing test

- **Isolation.** Two concurrent executions in one process: distinct
  `exec_id`s; a stop signal to one does NOT stop the other; their
  outcomes and metric rows are independently correct and separable by
  `exec_id`.
- **Single-run invariance (A1).** A representative `nmbrs run` produces
  byte-identical output/log/store with the task-local layer present but
  unused (no context scoped) — proves the override is a true no-op for
  the existing path.

## 9. The run-path factoring (the final Push-1 step)

`run_executions_concurrent` runs `ExecutionTask`s today; to run **workloads**
sharing **one** session it needs `run_impl` (`runner.rs`, ~2000 lines) split
into a once-per-session host and a per-execution run. The full shared-state
surface, mapped:

**Shared (session host — set up ONCE, then reused by every execution):**
| What | Site | Note |
|---|---|---|
| `Session` (dir, id, component root) | `runner.rs:1227` (`new_with_args`/`resume`) | the session polydat scope / common root (§2) |
| `session.log` writer | `:1281` (`set_log_file`) | OnceLock — naturally first-wins/shared |
| `session_root` | `session.rs:1288` (`set_session_root`) | shared common root (§2) |
| cadence tree + reporter | `:1739`–`:1746` | session-level metrics pulse |
| `MetricsQuery` + `set_global_query` + `install_live_access` | `:1747`–`:1758` | session-level query service |
| scheduler + sqlite reporter (metrics.db) | `:1762`+ | one store, `exec_id`-tagged rows |

**Per-execution (run ONCE per execution, inside its scoped context):**
| What | Site | Note |
|---|---|---|
| `exec_id` + `ExecutionContext` | (Push 1) | allocated, scoped |
| workload parse + polydat compile + config | `:1490`–`:1725` | per workload spec |
| `ExecCtx` build | `:2618` | carries `exec_id`, derives `session_component` from the host |
| pre-map (`execute_tree`, `pre_map_only`) | `:2705` | installs the execution's scene tree (already context-aware) |
| execution (`execute_tree`) | `:2897` | propagated fibers carry the context (Push 1) |

**Plan:** extract `SessionHost::setup(session_args) -> SessionHost` (the shared
table) and `run_execution(host, workload_args, ctx) -> ExecutionOutcome` (the
per-execution table, run inside `execution_context::scope`). `run_impl` becomes
`let host = SessionHost::setup(args)?; run_execution(&host, args, single_ctx)` —
preserving the single-run path exactly (one host, one execution). The
concurrent harness becomes `let host = SessionHost::setup(session_args)?;
run_executions_concurrent(specs.map(|s| run_execution(&host, s, ctx)), 10)`.

**Why a separate, careful pass:** this touches the hottest, most
scar-tissue-prone path (the run spine + the metrics/scheduler setup). Every
*accessor* it relies on is already context-aware and proven live end-to-end
(the headless run test), so the extraction is mechanical-but-large and best
done against a green tree with fresh context — not bundled into the plumbing
work. It carries no new design risk; the design is this section.

**LANDED (2026-06-21).** `run_impl` is now `let host =
SessionHost::setup(args, observer)?; let r = run_execution(&host, args,
observer).await; host.shutdown(); r`. `SessionHost::setup` is **workload-
independent** (session identity = `scenario=` param; metrics services +
profiler read CLI `params`); `run_execution` aliases the host's shared state,
loads/compiles its workload, derives the `Execution`, walks the scenario tree,
**flushes its metrics to the store via `CadenceReporter::quiesce`** (the
non-terminal per-execution flush-to-store — drain THIS execution's windows into
the sink without tearing the session-shared reporter down), and renders
summaries; `SessionHost::shutdown` does the session-tier teardown once
(scheduler stop + cadence `shutdown` drain+join + WAL consolidate + profiler).
`StopHandle::stop` became `&self` (interior-mutable) so the host stops the
scheduler through a shared `Arc`. A1 byte-identical (verified). **The bespoke
`run_executions_concurrent` semaphore is now superseded** — the next step roots
the execution graph at the session (first-class `ScopeKind::Session` +
`Execution` nodes; `execute_tree`/`ScheduleSpec` schedules execution-level
concurrency, `Bounded(1)`=serial), per SRD-02 One Concurrency Path, and retires
`concurrent.rs`.

## 10. References

- SRD-77 §"Entity model — Session vs Execution" — the model this extends.
- SRD-02 §"fiber task-local context" — the machinery the context rides.
- SRD-87 / SRD-41 — observer + output channel going per-execution.
- SRD-45 — the session = one durable store, now multi-writer.
