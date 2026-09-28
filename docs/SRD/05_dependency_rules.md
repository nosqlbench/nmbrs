# 05: Crate & Module Dependency Rules

Authoritative rules for which crates and modules may depend on which, and how.
This is **Pillar 5 (Enforced edges)** of the [Subsystem Treatment Standard](00b_subsystem_standard.md)
made system-wide: workspace-applicable edges are machine-checked by
[`crates/nmbrs/tests/architecture_rules.rs`](../../crates/nmbrs/tests/architecture_rules.rs) and run in CI;
Polydat-owned rules are enforced in its repository.
A proposal that contradicts a rule here is wrong, not the rule.

This SRD promotes and formalizes the informal "Dependency Rules" and "Contract
Boundaries" that previously lived in [SRD 01](01_system_overview.md).

---

## Dependency layers

The workspace is a DAG. Every workspace crate has a **layer**; an internal edge in `[dependencies]` may
only point to a **strictly lower** layer. (`[dev-dependencies]` may point upward — tests
legitimately pull in real adapters as fixtures.)

| Layer | Crates | Rationale |
|------:|--------|-----------|
| **L0** | `nmbrs-errorhandler` | leaf substrate; zero internal deps |
| **L1** | `nmbrs-metrics`, `nmbrs-workload` | consume the external `polydat` contract |
| **L2** | `nmbrs-rate`, `nmbrs-adapter-openapi`, `nmbrs-metricsql` | `rate`→`metrics`; `openapi`→`workload`; `metricsql`→`metrics` (the query language atop the metric query API, [SRD 40c](40c_metric_query_api.md)) |
| **L3** | `nmbrs-runtime` | the integration hub: `polydat` + `metrics` + `rate` + `errorhandler` + `workload` |
| **L4** | `nmbrs-adapter-{stdout,http,plotter,cql}`, `nmbrs-tui`, `nmbrs-web`, `nmbrs-optimizers` | implement / consume the activity contract (`nmbrs-optimizers` registers optimizer plugins via inventory — SRD-86) |
| **L5** | `nmbrs-adapter-testkit` | composite adapter (`→ stdout`) |
| **L6** | `nmbrs` (binary) | composition root; depends on everything |

`polydat` and `polydat-derive` are published, externally owned dependencies as of
version `0.2`; they are no longer workspace members and therefore are not assigned
workspace layers. Their source and design documentation live in the
[Polydat repository](https://github.com/nosqlbench/polydat).

`vendor/cassandra-cpp` is a vendored third-party fork and is exempt (it is `publish = false`
and outside the layer rules; only `nmbrs-adapter-cql` consumes it, feature-gated).

---

## Rules

- **D1 — polydat is external.** Workspace crates consume the published `polydat`
  contract and do not depend on an in-tree source copy. Polydat's own dependency
  boundary is enforced in its repository.
- **D2 — no upward edges.** Every `[dependencies]` edge points to a strictly lower layer.
  Subsumes "foundation crates never depend on the integration/presentation tier."
  (`tests::d2_edges_point_down`)
- **D3 — activity sits below presentation.** `nmbrs-runtime` (L3) depends only on L0–L2
  plus published external foundations;
  it never depends on an adapter, the TUI, the web UI, or the binary. (covered by D2)
- **D4 — adapters don't depend on adapters.** No adapter crate depends on another adapter,
  with the single allowlisted exception `nmbrs-adapter-testkit → nmbrs-adapter-stdout`
  (structured fall-through output). (`tests::d4_no_cross_adapter_edges`)
- **D5 — internal modules stay `pub(crate)`.** A **workspace-internal** crate's
  declared-internal modules are `pub(crate)`, so the **compiler itself** forbids any foreign
  crate reaching them — the surface can't re-grow by accident. `tests::d5_public_surface` is
  the regression guard (fails if one is re-widened to bare `pub`). **Scope:** D5 applies only
  to crates whose public API *is* "what the workspace consumes" (`nmbrs-runtime`, `nmbrs-tui`,
  `nmbrs-web`). **Standalone libraries** (`nmbrs-metricsql`, `nmbrs-rate`,
  `nmbrs-errorhandler`, `nmbrs-optimizers`) are **exempt** — their public API is their own library contract,
  legitimately broader than any single consumer; narrowing it would amputate real API.
  Modules used by a crate's *own* integration tests also stay `pub` (white-box tests are
  external crates) and are exempt.
- **D6 — no reaching past polydat's surface.** No workspace crate may `use` a deep
  polydat internal path (`polydat::compile::jit::…`, `polydat::library::support::…`).
  (`tests::d6_no_polydat_deep_paths`)
- **D7 — ownership follows the repository boundary.** Polydat implementation and
  substrate-design rules are owned and enforced by the external Polydat repository;
  nmbrs owns only its integration contract and may link to upstream documentation.
- **Discovery is by inventory, not wiring.** Adapters register via the
  `nmbrs_runtime` adapter inventory; polydat nodes via `polydat`'s `inventory`
  (emitted by `polydat-derive`); metric reporters via `nmbrs_metrics`. The binary
  discovers implementations at link time, not by hard-coded lists.

---

## Contract Registry

The single source of truth for Pillar 1 (Contract surface) and the input the CI gate
reads for D5. One row per workspace crate: **public surface** (what it exports), **inbound
contract** (what it requires from below), **allowed edges** (lower-layer crates it may
depend on).

| Crate | Public surface (exported) | Inbound contract (consumed) | Allowed edges |
|---|---|---|---|
| `nmbrs-metricsql` ([SRD 08](08_metricsql.md)) | **full library API** (`lexer`,`parser`,`prettifier`,`ast`,`query_rewrite`,`eval`,`streaming`,`grammar`,`runtime`, + the four `metricsql_*` polydat reader nodes under feature `polydat-nodes`) — reusable library, **D5-exempt** (own contract; no longer zero-dep) | a `MetricAccess` impl from `nmbrs-metrics::queryapi` ([SRD 40c](40c_metric_query_api.md)) | `nmbrs-metrics`; `polydat` (feature `polydat-nodes`) |
| `nmbrs-errorhandler` ([SRD 07](07_error_routing.md)) | `ErrorDetail`,`ErrorHandler`,`ErrorRouter` (re-exported), `handlers` | — | — |
| `nmbrs-optimizers` ([SRD 86](86_optimization.md)) | **full algorithm API** (`Optimizer`/`Objective`/`SearchSpace`/`algos` — the local trait + 9 optimizers), **D5-exempt**; the `runtime` feature adds the inventory **bridge** (`bridge.rs`) that registers each against the core contract. The contract itself lives in `nmbrs-runtime`, NOT here | the core `nmbrs_runtime::optimize` contract (only under the `runtime` feature) | `nmbrs-runtime` (runtime feature only) |
| `nmbrs-metrics` ([SRD 39](39_metrics_contract.md)) | `component`,`instruments`,`labels`,`selector`,`controls`,`snapshot`,`cadence`,`cadence_reporter`,`scheduler`,`summaries`,`metrics_query`,`reporters`,`diag`,`queryapi` (the metric query API: `MetricAccess` + `Vector`/`Series`/`Sample`/`Matcher` shapes + `MetricCatalog` + live/sqlite backends + `AccessProvider`, [SRD 40c](40c_metric_query_api.md)) — *internal: `validation`* | `polydat::{ast,kernel}` | `polydat` |
| `nmbrs-workload` ([SRD 25](25_workload_contract.md)) | `model`,`parse`,`bindpoints`,`tags`,`inline`,`catalog`,`edit`,`extends`,`report`,`metric_format`,`polydat_matter` — *internal: `template`,`spectest`* | `polydat::{ast,dsl}` | `polydat` |
| `nmbrs-rate` ([SRD 06](06_rate_limiter.md)) | `RateSpec`,`RateLimiter`,`RateLimiterApplier` (re-exported; modules private) | `nmbrs_metrics::controls` | `nmbrs-metrics` |
| `nmbrs-adapter-openapi` | `DriverAdapter` impl (inventory) | `nmbrs_workload::model` | `nmbrs-workload` |
| `nmbrs-runtime` | `adapter`,`op_modifier`,`wrapper_registry`/`wrapper_resolver`,`runner`,`activity`,`session`,`scene_tree`,`scope_tree`,`bindings`,`phase_outcome`,`phase_end_triggers`,`refine_plan`,`checkpoint`,`observer`,`diag!`,`readouts`,`lifecycle`,`log_sink`,`wires`,`polydat_nodes`,`resource_pool`,`optimize`(the SRD-86 optimizer **contract** + registry),`fixture`(test) — **full surface + the ~25-module internal narrowing target in [SRD 29](29_execution_engine.md)** | `polydat`,`nmbrs-metrics`,`nmbrs-rate`,`nmbrs-errorhandler`,`nmbrs-workload` | L0–L2 plus external `polydat` |
| `nmbrs-adapter-{stdout,http,plotter,cql}` | `DriverAdapter`/`OpDispenser` impls (inventory) | `nmbrs_runtime::adapter` | `nmbrs-runtime`,`nmbrs-workload`,`polydat`(+`metrics`/`cassandra-cpp` for cql) |
| `nmbrs-tui` ([SRD 59](59_tui_contract.md)) | `observer`(`TuiObserver`),`state`,`run_state_actor`, sink seam (`display_sink`/`log_only_sink`/…), inspector (`inspector_server`/`repl_state`/`key_watcher`) — *internal: `app`/`widgets`/`reporter`/`frame_broker`/`prompt_state`/`readout_panel`/`readout_sink`/`tui_sink`* | `nmbrs_runtime::observer`, `nmbrs_metrics` | `nmbrs-runtime`,`nmbrs-metrics`,`polydat` |
| `nmbrs-web` ([SRD 54](54_web_ui.md)) | `server`,`ws` — *internal: `routes`,`models`,`graph`* | `nmbrs_runtime`, `nmbrs_metrics` | `nmbrs-runtime`,`nmbrs-metrics`,`polydat` |
| `nmbrs-adapter-testkit` | `DriverAdapter` impl (inventory) | `nmbrs_runtime::adapter` | `nmbrs-runtime`,`nmbrs-adapter-stdout`,`nmbrs-workload`,`polydat` |
| `nmbrs` | — (binary) | all of the above | all (L0–L5) plus external dependencies |

> `nmbrs-runtime` exposed 51 modules; **18 are now `pub(crate)`** (the dispatch/synthesis
> machinery — see [SRD 29 §Internal](29_execution_engine.md); three superseded modules —
> `cycle`/`binder`/`linearize` — were deleted as dead code the narrowing revealed). Four more (`scope_synth`,
> `scope`, `wrappers`, `validation`) stay `pub` only because the crate's own integration
> tests use them. D5 is **enforced** — the compiler walls off the `pub(crate)` set and
> `tests::d5_public_surface` guards against re-widening. Likewise `nmbrs-tui` (6 narrowed).
> (`nmbrs-metricsql` is D5-exempt — its public API is its own library contract — but it is
> **L2**, depending on `nmbrs-metrics` for the data-access seam ([SRD 40c](40c_metric_query_api.md));
> "extractable" here means own-contract, not zero-dep, exactly as for `nmbrs-rate`.)

---

## See also
- [SRD 00b — Subsystem Treatment Standard](00b_subsystem_standard.md) (Pillar 5)
- [SRD 01 — System Overview](01_system_overview.md) (crate map, data flow)
- [`crates/nmbrs/tests/architecture_rules.rs`](../../crates/nmbrs/tests/architecture_rules.rs) (the gate)
