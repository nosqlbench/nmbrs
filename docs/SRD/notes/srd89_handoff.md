# Hand-off: SRD-89 control rooting (Push 1 retry)

> ## ⚠️ 2026-06-22 (later session) — THIS HAND-OFF'S DIAGNOSIS IS LARGELY REFUTED
>
> Read **first**: memory `project_srd89_execution_scoped_resolution.md` (the
> "Push 1 SHIPPED + handoff causal model REFUTED" block). Corrections, all
> code-verified/experiment-verified:
> - **Push 1 control rooting is DONE + correct + validated** (per-exec lock-free
>   `controls` map on `ExecutionContext`, populated PHASE-LOCAL — NOT the
>   `control_snapshot` session-root up-walk of SRD §3c-i. Cross-talk + A1 unit
>   tests pass).
> - **There is NO per-cycle component write lock.** The "writer-preferring RwLock
>   deadlock vs cadence instrument-registration" story (incl. the §"Two attempts"
>   below and the `component.rs:539` comment) is **FALSE** — `capture_recursive`
>   is read-only.
> - The "third hang" was **`dynamic_controls.yaml`'s feedback loop** (which Push 1
>   newly activated) collapsing `rate`→0 under concurrency → rate-limiter stall.
>   FIXED in the example (floor the rate, read it as f64). NOT a lock issue.
> - **control/multiservo are STILL RED, and control rooting does NOT fix them.**
>   Real blocker: `metricsql_scalar("sum(rate(errors_total[400ms]))")` returns
>   **0 series under concurrency** (works solo) → settle stabilises at err_rate=0
>   for all coords → wrong `best`. RULED OUT: control rooting, the exec_id filter,
>   saturation type. It's the **windowed metric read empty/stale under the shared
>   cadence reporter** — the metric-side push (§3b/§4 Push 4) THIS hand-off told
>   you to skip ("metrics already scope"). They DON'T, for `metricsql_*`. That is
>   the actual walker-green work. Everything below is the OLD (wrong) framing.

**Date:** 2026-06-22. **For:** the next session picking up SRD-89 Push 1.
**Read this first, then SRD-89.** Everything below is code-verified this
session unless marked otherwise. Line numbers drift — grep the symbol.

---

## TL;DR

**Goal:** the example walker passes `control` + `multiservo` under
concurrency (no serial isolation), at **1 hardware thread / 20 concurrent
executions**.

**State:** tree is functional. Only `control`/`multiservo` are red in the
walker (the known cross-talk). Everything else green.

**Do this:** take **approach (2)** below — capture each control's handle at
**declaration time** into a per-execution lock-free map; route `control(...)`
reads at it. **Never walk the live component tree under concurrency** — that
hazard class has already cost two reverts (deadlock, then an unpinned hang).

**Reading order:** (1) this file → (2) `docs/SRD/89_execution_scoped_resolution.md`
(§2b, §3c, §3c-i incl. the "Push-1 attempt finding", §4) → (3) `docs/SRD/88_concurrent_executions.md`
§3c → (4) memory `project_srd89_execution_scoped_resolution.md`.

---

## The confirmed diagnosis (code-verified, do not re-derive)

The servo examples converge wrong under concurrency. Root cause chain:

1. **Servo control WRITE is already per-execution.** `optimize/servo.rs::retarget`
   resolves the control off the **phase component** (`phase_component.find_control_erased_up`).
   Not the bug.
2. **`control()` / `control_u64()` READ is NOT per-execution.** In
   `nmbrs-runtime/src/polydat_nodes/runtime_context.rs`, `control_gauge_f64` →
   `session_root()` → the process-global `SESSION_ROOT` (the **session** root,
   set in `session.rs`). It calls `find_control_erased_up`, which only walks
   **up**. The concurrency control lives on the per-exec **phase component** (a
   *descendant* of session) — so the up-walk from the session root **never
   finds it** → returns stale/0. **This is the gap.** (It's also why
   `control_u64("concurrency")` "returned a constant" earlier.)
3. **Metric reads are already exec-scoped** (`read_exec_id=Some` per exec; the
   `select_range` exec_id post-filter works). Not the bug.
4. **Measured saturation is host-dependent** — under concurrent in-process
   execution it produces no reliable overload signal, which is *why* the
   measured optimizer examples go red. The intended fix is **synthetic
   saturation** (`load := control_u64("concurrency")`), which is blocked on
   the control-read gap (#2).

So: fix the control READ to resolve per-exec (= "control rooting"), then
re-apply synthetic saturation (follow-on, below).

---

## Map (symbols, not just lines)

| What | Where |
|---|---|
| Control read/write nodes | `nmbrs-runtime/src/polydat_nodes/runtime_context.rs`: `control_gauge_f64`, `control_value_string`, `control_set`, `FiberContext`, `with_fiber_context`, `session_root()`/`SESSION_ROOT` |
| Per-exec component | `nmbrs-runtime/src/session.rs` `Execution::component` (child of session, labels `exec_id`+`workload`); becomes `ExecCtx.session_component` (`runner.rs` ~2793 `session_component: execution.component.clone()`) |
| Phase component | `nmbrs-runtime/src/executor.rs` ~4057-4060: `phase_component` attached to `ctx.session_component`; controls declared on it |
| Control declaration | `activity.rs::attach_component` (concurrency/rate) + `activity.rs::declare_adapter_controls`; `executor.rs` ~4110 `declare_adapter_controls(&adapters, &phase_component)` |
| Servo write | `nmbrs-runtime/src/optimize/servo.rs::retarget` (off `phase_component`) |
| Non-nested snapshot helper (KEPT, reusable) | `nmbrs-metrics/src/component.rs::Component::control_snapshot` (~542) |
| Control registry / handle | `nmbrs-metrics/src/controls.rs`: `ControlRegistry` (`list() -> Vec<Arc<dyn ErasedControl>>`, `get_erased`, `declare`); `trait ErasedControl` (`name`, `gauge_f64`, `value_string`, `branch_scope`, `set_f64`) |
| Walker test | `nmbrs/tests/example_workloads_in_process.rs` — `group_concurrent = max_concurrent` (~219; isolation already dropped, gap documented in-code); env `NMBRS_TEST_WORKER_THREADS` / `NMBRS_TEST_CONCURRENCY` / `NMBRS_TEST_EXAMPLES_DIR` |
| Optimizer examples (currently MEASURED) | `nmbrs/examples/workloads/optimizer/{control,multiservo,saturation,metricsql,hybrid}.yaml` |

---

## Two attempts already made — DO NOT repeat

1. **Nested-walk read-rooting** (reverted): routed `control_gauge_f64` through
   `phase_component.find_control_erased_up` **per cycle**. → writer-preferring
   `std::RwLock` **deadlock** — the per-cycle nested up-walk reads starve
   against the cadence path's instrument-registration writes on the same
   component. Linux `std::RwLock` is writer-preferring; a nested read while a
   writer waits deadlocks.

2. **Lock-free snapshot wiring** (reverted): built `Component::control_snapshot`
   once per phase, carried on `FiberContext`, reads via it (snapshot →
   `session_root` fallback). Validated SOLO (`control.yaml` → `best [2]`). A
   file-marker probe proved **the build itself is fine — 311/311 builds
   completed** under the concurrent walker (the non-nested walk does NOT
   deadlock). But the wired-in version **still hung the concurrent walker
   (>120s vs ~4s), including for measured examples that read no control** — so
   the hang is **neither the build nor the reads**, and I could not pin it.

The `Component::control_snapshot` helper is **left in the tree** — it's correct
and reusable.

---

## Recommended next approach — (2) declaration-time capture

Capture handles where they're **declared**, never walk the live tree under
concurrency:

1. When a control is declared (`attach_component`, `declare_adapter_controls`),
   also insert its `Arc<dyn ErasedControl>` (keyed by name) into a
   **per-execution lock-free map** (immutable once the phase's controls are
   declared, or an `ArcSwap`/append-only structure).
2. `control_gauge_f64` / `control_value_string` / `control_set` look the name
   up in that per-exec map (no component lock, no walk). Fall back to
   `SESSION_ROOT` only with no exec context (single-run / dryrun).
3. The servo already writes the same handle; reads see its retargets.

**Open design Qs to settle first:**
- Where the per-exec map lives — on `ExecutionContext` (task-local, same crate
  as the read nodes — convenient), or a per-exec struct threaded in.
- How the read nodes reach it (task-local read, like `exec_id` today).
- Single-run: a fallback `SESSION_ROOT` map, or give single-run a map too
  (SRD-89 §3d wants single-run = multi-run).

**Why (2) over pinning the hang:** the component `RwLock` under concurrent
in-process has now produced 3 stalls; (2) structurally never touches it. BUT
the snapshot-wiring hang was **unpinned**, so even (2) **must be validated
against the walker at 1/20** — the stall could (less likely) be elsewhere.

If you'd rather pin first: instrument with a **per-fiber + cadence marker
trace** (file-append, NOT eprintln — the walker captures per-exec stderr),
gated on an env var, written to `concat!(env!("CARGO_MANIFEST_DIR"),
"/../target/test-tmp/scope_diag.log")`.

---

## Reproduce + validate

- **Functional baseline:** `cargo test -p nmbrs --test example_workloads_in_process`
  → `75 passed, 2 skipped, 2 failed` (the 2 = `control`/`multiservo`), ~4s.
- **Load-bearing target:** `NMBRS_TEST_WORKER_THREADS=1 NMBRS_TEST_CONCURRENCY=20
  cargo test -p nmbrs --test example_workloads_in_process` → every optimizer
  example finds the correct `best`, deterministic across runs.
- **GOTCHA:** narrowed / low-concurrency probe runs **time out** (5 optimizer
  examples run serially exceed ~150s) — an earlier "reads fire 0×" conclusion
  was a **timeout artifact**. Use the **full** walker with a generous timeout
  (300s) for complete results; don't trust partial narrowed runs.
- **Solo check:** `./target/debug/nmbrs run workload=nmbrs/examples/workloads/optimizer/control.yaml --session-path target/test-tmp/<uniq>`
  → expect `best [2]`.
- **Two-exec cross-talk unit** (worth adding): two `control.yaml` execs in one
  session, distinct retargets → distinct correct bests.

---

## Follow-on (after control rooting is green)

1. **Synthetic saturation re-apply** (SRD-89 §4 Push 2): add
   `load := control_u64("concurrency")` + op-field `result-load: "{load}"`,
   `result-overload: 4` to the optimizer examples (switch from measured). With
   per-exec control reads, `control_u64` tracks the live setting →
   deterministic, host-independent.
2. **multiservo sub-issue:** with synthetic, `multiservo` (`servo:
   [concurrency, rate]`) read `[32,…]` solo — the conc=32 grid point didn't
   overload, i.e. `control_u64` didn't reflect 32 there. Diagnose the
   multi-axis retarget/read ordering before applying synthetic to it.
3. The metric-side pushes (SRD-89 §4 Pushes 3-5: matcher injection, single-run
   parity) are **orthogonal correctness/principle refinements** — metrics
   already scope; they do NOT block the walker.

---

## Constraints / gotchas

- **No git ops** — the user runs git. No `Co-Authored-By` trailer.
- **Project tmpdir** is `target/test-tmp` (`TMPDIR` redirected via
  `.cargo/config.toml`). Never `/tmp/foo`. Tests must set sandbox cwd +
  `--session-path` and never run from the project root.
- **`std::RwLock` is writer-preferring on Linux** — nested reads deadlock when
  a writer waits. The core hazard here.
- **Persona: greenfield** — make the most-correct change; backwards-compat is
  not a constraint.
- **Don't re-touch these (shipped + green this session):** the 20-node
  `#[polydat::polydat_node]` macro conversion; `temporal_window_ms` removal
  (settle now relies on stability detection alone); async `quiesce` /
  `shutdown_flush` + the flavor-gated `wait_for_done` in `cadence_reporter.rs`
  / `scheduler.rs` (fixed `checkpoint_resume_staircase`).
