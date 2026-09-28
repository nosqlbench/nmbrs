# 29: Execution Engine — Contract & Axioms (nmbrs-runtime)

The front door for **nmbrs-runtime**, the integration hub (layer L4): the async
dispatch loop, the adapter trait, op sequencing, scenario-tree walking, error/stop
handling, observation, and the runner. It depends on every foundation crate
(`polydat` + `nmbrs-metrics` + `nmbrs-rate` + `nmbrs-errorhandler` + `nmbrs-workload`) and
is consumed by every adapter, the TUI, the web UI, and the binary.

This document carries Pillars 1 (Contract) and 2 (Axioms) of the
[Subsystem Treatment Standard](00b_subsystem_standard.md) for the engine; the detailed
mechanism docs (SRD 30–35, 68, 71–76, 82–83) are the Pillar-3 tier beneath it.

---

## Contract

### Public surface (what nmbrs consumes)

The authoritative surface + edges are in [SRD 05 §Contract Registry](05_dependency_rules.md);
grouped here by consumption role for navigation. `nmbrs-runtime` declares 51 public modules
today, but the surface other crates actually depend on is the set below. New cross-crate use
should stay within it.

| Role | Surface |
|---|---|
| **Adapter contract** | `adapter` (`DriverAdapter`, `OpDispenser`, `OpResult`, `ExecutionError`, `ExecCtx`), `op_modifier`, `wrapper_registry`, `wrapper_resolver` |
| **Orchestration** | `runner::Runner`, `activity::Activity`, `session`, `session_signals`, `scene_tree`, `scope_tree`, `bindings`, `phase_outcome`, `phase_end_triggers`, `refine_plan`, `checkpoint` |
| **Observation / diagnostics** | `observer` (`RunObserver`, `LogLevel`, `log`), the `diag!` macro, `readouts`, `lifecycle`, `log_sink` |
| **Resolution / wires** | `wires` (`WireSource`), `polydat_nodes`, `resource_pool` |
| **Test fixtures** *(test-only)* | `fixture` (used by other crates' tests; not a runtime surface) |

### Inbound contract (what it requires)

`polydat` (kernel + `compile_polydat` + `PolydatMatter`), `nmbrs-metrics` (component tree,
instruments, controls), `nmbrs-rate` (`RateLimiter`), `nmbrs-errorhandler` (`ErrorRouter`),
`nmbrs-workload` (`Workload`, `ParsedOp`). Allowed edges: L0–L3 only — see
[SRD 05 §Contract Registry](05_dependency_rules.md).

### Internal — now `pub(crate)` (compiler-enforced)

These 18 dispatch/synthesis modules are `pub(crate)`; no foreign crate can reach them:

`adapters` (inventory glue), `params`, `scope_elision`, `phase_filter`,
`phase_params`, `scheduler`, `profiler`, `trace_router`, `executor`, `error_policy`,
`stop_conditions`, `workload_shell`, `describe`, `wrapper_registrations`, `relevancy`,
`fiber_pool`, `daemon_pool`, `readout_context`.

(The `cycle`, `binder`, and `linearize` modules were deleted as superseded dead code —
`cycle` replaced by `DataSourceFactory`, `binder` by the SRD-68 `ctx.wires`/`WireSource`
route, `linearize`'s dependency analysis never wired in.)

Four more — `scope_synth`, `scope`, `wrappers`, `validation` — stay `pub` **only** because
the crate's own integration tests (`tests/srd71_scope_tree_probe.rs`, `tests/fixture_strict.rs`,
`tests/recall_e2e.rs`) are white-box and reach them. (`synthesis`, `opseq`, `scope_tree`,
`report_anchor` are genuinely consumed by other crates and stay public.)

> Rule **D5** is now **enforced**: the compiler walls off the `pub(crate)` set, and
> `tests::d5_public_surface` ([SRD 05](05_dependency_rules.md)) is the regression guard
> that fails if any of these is re-widened to bare `pub`.

---

## Axioms

The load-bearing invariants the engine is built on. A design proposal that contradicts
one of these is wrong, not the rule.

- **A1 — One walker.** Pre-map, dryrun, and runtime are **one** tree walker at different
  depths; serial and concurrent are **one** harness at different limits. There is no
  "mode" flag that switches mechanism. [SRD 18b §Single Walker Contract], [SRD 02 §One
  Concurrency Path]. (`executor`, `scheduler`, `scene_tree`)
- **A2 — No blocking primitives in async.** A synchronous `recv()` / `lock()` / `sleep()`
  inside an async context starves the tokio runtime. Use async equivalents; offload
  genuinely blocking work. [SRD 02 §No Blocking Primitives in Async Contexts].
- **A3 — One recursive execution shell; two orthogonal outcome axes.** Scenario graph,
  phase, stanza, and op are one shell (body + policy + outcome) differing only in
  granularity. A result is `(Disposition × Validity)`, never a single status. **Error
  handling** (`error_policy` — a per-op router) is orthogonal to **stop conditions**
  (`stop_conditions` — predicate-triggered). [SRD 82], [SRD 83], extends [SRD 03] + [SRD 76].
- **A4 — The dispenser owns its canonical Polydat context.** One resolution surface per
  dispenser; wrappers read and write through `ctx.wires`, never a side channel. The
  narrow `wires::WireSource` trait walls adapter code off from kernel internals. [SRD 68].
- **A5 — One scope per scenario node.** Each non-trivial scenario-tree node is its own GK
  scope; iteration variables are scope outputs, not text substitution; a leaf phase
  compiles once. [SRD 18b]. (`scope_tree`, `scope_synth`, `bindings`)
- **A6 — The console belongs to the adapter; signals route through the observer/sink.**
  A `println!` / `eprintln!` of a system signal is a bug — it bypasses the sink,
  `session.log`, and the TUI. Emit via `observer::log` / `diag!`. [SRD 41 §Output
  Routing], [SRD 30 §Display Preference].
- **A7 — Resource lifecycle is pre-map-driven.** The walker predicts phase attach/detach;
  shared / per-scenario driver resources close on last-predicted-phase-detach (multi-
  generation refcount) with an explicit async `close()`. [SRD 35]. (`resource_pool`)
- **A8 — Display surfaces project one typed event stream.** `lifecycle` / `readouts`
  emit typed events; the `RunState` snapshot is a fold; no surface consumes a string
  rendered for another surface. [SRD 81].

---

## Mechanism tier (Pillar 3)

The detailed specs beneath this front door:

| SRD | Topic | Modules |
|---|---|---|
| [30](30_adapter_interface.md) | Adapter / OpDispenser contract | `adapter` |
| [31](31_op_pipeline.md) | Op execution pipeline | `executor`, `opseq` |
| [32](32_wrappers.md) / [32a](32a_wrapper_registry.md) | Dispenser wrappers, registry, stacking | `wrappers`, `wrapper_registry`, `wrapper_resolver` |
| [33](33_result_validation.md) / [34](34_capture_points.md) | Validation, capture | `relevancy`, `wires` |
| [35](35_driver_resources.md) | Driver resource lifecycle | `resource_pool` |
| [68](68_dispenser_owned_polydat_context.md) | Dispenser-owned context, `WireSource` | `wires`, `synthesis`, `bindings` |
| [71](71_cursor_partitions.md) | Cursor partitioning | `phase_params` |
| [73](73_op_field_modifiers.md) | Op field modifiers | `op_modifier` |
| [75](75_phase_poll.md) | Phase-level poll | `executor` |
| [76](76_phase_outcome_disposition.md) | Phase outcome disposition | `phase_outcome` |
| [82](82_uniform_execution_shells.md) / [83](83_stop_conditions.md) | Execution shells, stop conditions | `error_policy`, `stop_conditions` |

Scenario-tree / scope / scheduler structure is [SRD 18b](18b_scenario_tree_and_scheduler.md)
(shared with the workload model).

---

## Runtime context nodes (DSL surface)

The engine registers a set of Polydat library nodes (in `nmbrs-runtime::polydat_nodes`)
that project nmbrs runtime state into the DSL — the host-registered nodes the open
polydat registry allows (polydat itself provides only deterministic nodes; these depend
on the component tree / executor / controls, which is why they live here, not in
polydat). Each projects a single runtime surface into a wire — no side channels, no
templating hooks. The engine marks them `volatile` so the constant-folder leaves them
in place.

| Node | Signature | Description |
|------|-----------|-------------|
| `control` | `String → f64` | Current committed value of a [dynamic control](23_dynamic_controls.md) by name, via its reified gauge. Resolves by walking up the component tree from the session root, honoring branch scope. Missing / non-reified / non-numeric → `0.0`. |
| `control_u64` | `String → u64` | As `control`, cast to `u64` (negatives clamp to `0`). Sugar over `f64_to_u64(control(name))`. |
| `control_bool` | `String → bool` | As `control`, `true` iff the gauge value is non-zero. Missing → `false`. |
| `control_str` | `String → String` | As `control`, rendered via the control's erased `value_string()`. For enum / string-valued controls. |
| `control_set` | `String, f64 → u64` | Non-blocking write into a named control (spawns an async `set_f64`). Returns `1` if dispatched, `0` if no session root. `Versioned<T>::origin` carries the enclosing binding name. |
| `metric` | `String → f64` | Latest reading of a named metric series, scoped to the nearest ancestor component publishing it. Pairs with `metric_window(name, duration)` ([SRD 42](42_windowed_metrics.md)). |
| `phase` | `→ String` | Name of the executing phase; pins against the enclosing executor (`tokio::task_local!`, so work-stealing can't leak phase identity across fibers). |
| `cycle` | `→ u64` | Current cycle ordinal for the running fiber — sugar for reaching the cycle value without declaring it as an explicit input. |
| `concurrency` | `→ f64` | Alias for `control("concurrency")` — the activity's live fiber count. |
| `rate` | `→ f64` | Alias for `control("rate")` — the live rate-limiter target (ops/sec). |

Writes go through `control_set` ([SRD 23](23_dynamic_controls.md)); read-side nodes are
side-effect-free but `volatile` (value changes per cycle, so constant-folding is illegal).
Adding new mutable runtime state: attach it to the component it governs, decide read-only
projection (context node) vs writable (control), and register it so authors see it by name
in `--explain` / `dryrun=controls`. The reification framing is [SRD 10 §"GK as the unified
access surface"](10_polydat_language.md); the open-registry mechanism is
[polydat `library_catalog.md` §Host-registered nodes](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/library_catalog.md).

---

## See also (Pillar 4)
- crate root: `crates/nmbrs-runtime/src/lib.rs` (module doc) — `runner::Runner` is the entry point
- tests: `crates/nmbrs-runtime/tests/`, `crates/nmbrs/tests/op_composition_dryrun.rs`
- [SRD 00b — Subsystem Treatment Standard](00b_subsystem_standard.md) (the rubric)
- [SRD 05 — Dependency Rules](05_dependency_rules.md) (the enforced edges + Contract Registry)
- [SRD 01 — System Overview](01_system_overview.md)
