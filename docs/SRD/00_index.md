# nmbrs System Reference (SRDv2)

Consolidated system reference for nmbrs. Superseded memos and shipped
implementation plans are archived under [`history/`](history); living
design rationale is under [`notes/`](notes).

This reference is organized by subsystem, not by chronological
discovery order. Each document is self-contained and
cross-referenced. The source of truth is the code; this reference
explains the design intent behind it.

Every subsystem here is held to the
[Subsystem Treatment Standard](00b_subsystem_standard.md) — a Contract
surface, axiom-tier invariants, mechanism detail, cross-references, and
machine-checked dependency edges. **polydat** is the worked exemplar;
its substrate design lives in the external
[Polydat repository](https://github.com/nosqlbench/polydat/tree/main/crates/polydat/docs).

---

## Documents

### 1. System Architecture

| # | Document | Scope |
|---|----------|-------|
| 00b | [Subsystem Treatment Standard](00b_subsystem_standard.md) | The five-pillar rubric every subsystem is held to; polydat as worked exemplar; per-subsystem compliance matrix |
| 01 | [System Overview](01_system_overview.md) | Crate map, data flow, build structure |
| 02 | [Concurrency Model](02_concurrency_model.md) | Async fibers, tokio runtime, cycle source, rate limiting |
| 03 | [Error Handling](03_error_handling.md) | Error scoping, routing, retry semantics, silent failure policy |
| 05 | [Crate & Module Dependency Rules](05_dependency_rules.md) | Dependency layers (L0–L6), the Contract Registry, no-upward-imports, external Polydat boundary; workspace edges are **machine-checked** by `crates/nmbrs/tests/architecture_rules.rs`. Pillar 5 of the Subsystem Treatment Standard |
| 06 | [Rate Limiter — Contract & Axioms](06_rate_limiter.md) | Front door for **nmbrs-rate**: tight 3-type surface, async-non-blocking acquire, live retarget |
| 07 | [Error Routing — Contract & Axioms](07_error_routing.md) | Front door for **nmbrs-errorhandler**: pattern→handler-chain router, every-error-routed, standalone |
| 82 | [Uniform Execution Shells](82_uniform_execution_shells.md) *(DRAFT)* | Scenario graph / phase / stanza / op as one recursive execution shell (body + policy + outcome); two-axis `Outcome` (`Disposition` × `Validity`) replacing `PhaseStatus`; `ErrorPolicy` = per-op router that is also its own resolver (depth-inherit / breadth-share, value-equality, scope-init bound); `stop` cause `Interrupt` vs `Fault`. Extends SRD-03 + SRD-76. Stop conditions are orthogonal — SRD-83 |
| 83 | [Stop Conditions](83_stop_conditions.md) *(DRAFT)* | Predicate-triggered, daemon-scheduled stop conditions, orthogonal to SRD-82 error handling. `(when: polydat predicate over runtime-state wires, trigger: firing-event enum or named backoff, effect: two-axis Outcome)`. Triggered (not continuous) evaluation; a standard `settle` daemon fires eagerly then backs off. Error-rate breach + stop-on-error become default stop conditions; retires the `AggregateGuard` stopgap |
| 87 | [Output Channel](87_output_channel.md) *(DRAFT)* | The single `OutputChannel` transport seam: every byte to the **one user terminal** is a typed **bucket** submission (op-output / log / status / raster) to one swappable, non-blocking, **sole-fd-owner** conduit. Generalizes the Readout notify/observe/project template to *all* output; consolidates the output halves of `RunObserver` + `DisplaySink` + the free `op_output()`. Supersedes the **transport** halves of SRD-41/30/52; **preserves** SRD-02's fold and SRD-63/81 as the content/spine layers above it. Fixes the stdout-prints-nothing-on-an-interactive-TTY defect as a consequence |
| 88 | [Concurrent In-Process Executions](88_concurrent_executions.md) *(DRAFT)* | Extends SRD-77's `Session ⊃ Execution[]` model with **concurrency**: N executions running at once, in one process, sharing one session. Load-bearing seam is a **task-local `ExecutionContext`** that replaces the process-global "one run per process" singletons (`GLOBAL_OBSERVER`, `CHANNEL`, `GLOBAL_TREE`, `SESSION_ROOT`, `SESSION_STOP`, the log-level statics) — **additively** (axiom A1: process-globals stay as the default; single-run is byte-identical). One `exec_id` allocator is the only new global (A2). Durable stores stay one-per-session, `exec_id`-tagged (A3). Headless-first; concurrent live display deferred |

### 2. Polydat Kernel (polydat)

polydat is the external substrate **exemplar**: its authoritative substrate design
lives in the [Polydat repository](https://github.com/nosqlbench/polydat/tree/main/crates/polydat/docs)
(axiom + mechanism tiers). The documents below are the
**nmbrs-side** integration view — start at [09 Polydat Contract](09_polydat_contract.md)
for the public surface nmbrs consumes and the map into the upstream docs. Several entries
(12, 13c, 16, 16b, 67, 74) are redirect stubs to their polydat home.

| # | Document | Scope |
|---|----------|-------|
| 09 | [Polydat Contract Surface](09_polydat_contract.md) | **Front door:** the public boundary nmbrs consumes (`PolydatKernel`, `Value`, `compile_polydat`, `PolydatMatter`, `audit`, …) + the map from each polydat-integration SRD to its authoritative `polydat/docs/` home. Pillar 1 for the exemplar |
| 10 | [GK Language and Compilation](10_polydat_language.md) | nmbrs-side framing only (output selection, unified state holder, op-level bindings); **the definitive surface-language grammar spec+guide is [polydat_grammar.md](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/polydat_grammar.md)**, with **Polydat as the unified access surface for runtime state** |
| 11 | [GK Evaluation Model](11_polydat_evaluation.md) | Kernel/state split, input spaces, two lifecycles (effectively-const / dynamic), provenance-based invalidation, const-binding contract |
| 12 | [GK Standard Library](12_polydat_stdlib.md) | Node catalog, type signatures, P3 JIT eligibility, runtime context nodes |
| 13 | [GK Modules](13_polydat_modules.md) | File-based modules, inlining resolution, compiler diagnostic event stream |
| 13b | [GK Combination Modes](13b_polydat_combination_modes.md) | Four-mode taxonomy: inline, scope composition, subgraph, reification |
| 13c | [GK Scope Model](13c_polydat_scope_model.md) | Scope hierarchy, visibility / mutability rules, `for_each` lifecycle, auto-extern composition, the scope-composition mode in depth |
| 13d | [Op-template Polydat Scope Layer](13d_op_template_scope.md) *(SKETCH — not yet implemented)* | Op templates as Polydat scopes; **`HasPolydatMatter` trait** declarative classification (None / Readonly / Definitions) every workload-AST node implements; **scope elision** (trait short-circuit + program-hash equivalence) collapses trivial scopes into the parent; **subcontext symbol redefinition forbidden by default**; staged **realisation lifecycle** (Source → AST → resolvable AST → program → hash check → instance) with two cache layers and three short-circuits; **walking parent-kernel reference** (`nearest_materialised`) and **logical kernel names** for diagnostics; `dryrun=op` diagnostic level + `nmbrs describe polydat` integration; dedicated proving-out test suite. Prerequisite for SRD-40b |
| 13e | [Scope-as-Module Refinement](13e_scope_as_module.md) *(DESIGN — not yet implemented)* | Promotes every sub-scope (phase, op-template, comprehension, do-loop) to a formal `ScopeModule` with typed import / export contracts; `ScopeModule::instance_under(parent)` is the single typed attach operation that replaces ad-hoc string-concat synthesis + `bind_outer_scope` + manual scope-value reapply; `ScopeContract` issues kernel-bound `ImportHandle<M>` / `ExportHandle<M>` so cross-kernel mis-routes are ill-typed at compile; refinement of 13c/13d. Migration plan in §5; absorbs the SRD-13d Phase 9 followup architectural fixes (Coordinate skip, workload-param precedence, owning-phase resolution, post-bind init pull) into automatic consequences of the typed contract. |
| 13f | [Cross-Scope Wire Materialization](13f_cross_scope_wire_materialization.md) | Matter-AST-driven materialization at subscope construction: inlined constant (compile-time fold) / value-only shared cell w/ valid bit / read-write shared cell (mutex). Workload-param indirection through the params-kernel so scenario-tree `bindings:` / `set:` shadow upstream values via local `const` / `const`. Local-authoritative shadow (transit suppression) rule in `materialize_wiring_from_outer` enforces lexical scoping uniformly across every scope-tree node. |
| 14 | [GK Config Expressions](14_polydat_config_expressions.md) | `{...}` form: init-time constants flowing into activity config |
| 15 | [Strict Mode](15_strict_mode.md) | Compile-time enforcement: config wire promotion, explicit declarations, no silent coercions |
| 16 | [GK Engines](16_polydat_engines.md) | Compilation levels P1/P2/P3, provenance push/pull, engine variants, auto-selection heuristic |
| 16b | [GK JIT Wiring](16b_polydat_jit.md) | Cranelift ↔ Rust call boundary, setjmp/longjmp for catchable predicate violations, hybrid-kernel wrapping, `invoke_with_catch` contract |
| 105 | [JIT as Default Engine: Auto-Mixed Cones](105_jit_default_engine.md) | Maximal JIT-eligible cone extraction at assembly time, synthetic cone nodes in interpreter DAG, `JitMode` configuration |
| 110 | [Full Cranelift Optimization Plan](110_full_cranelift_optimization_plan.md) | Inventory of all 677 Polydat functions, Cranelift JIT promotion roadmap (comparisons, selections, conversions, variadics) |
| 111 | [Non-Scalar JIT & Universal Lowering Architecture](111_non_scalar_jit_architecture.md) | Architectural foundation for lowering Strings, JSON, Bytes, Digest, and Datafile nodes to Phase 2/3 via cycle bump arenas and handle interning |
| 112 | [Optimization Strategies & Concurrency Baseline](112_optimization_strategies_and_baseline_benchmarks.md) | Multi-tier optimization strategies (P1/P2/P3), micro-architectural differences, and empirical multi-concurrency (C=1, 4, 16) baseline benchmark metrics |
| 79 | [Type-Driven Name Resolution](79_type_driven_name_resolution.md) *(DRAFT — not yet implemented)* | Three-phase refactor whose **primary goal is primitive type alignment end-to-end** (`U64`, `F64`, `Str`, `Bool`, `VecU64`, `VecF32`, …): the type-expectation graph collapses workload-author polymorphism into runtime specialization wherever the alignment can be proved safe, no intermediate adapters needed. The **type fusion / polyfill layer** is the secondary mechanism — it (a) bridges primitive-to-primitive mismatches (`u64_to_f64` widening, `parse_u64` from string, etc.) and (b) materializes primitive types from `PortType::Json` at receiver sites via `as_*` extractors. `Value::Json` is the **interstitial bridge and last-resort carrier** — used only in two narrow cases: degenerate-typing cases the graph can't decide (mixed-type arrays, heterogeneous objects, cross-consumer conflict), and operator-declared deferred typing where the value is explicitly meant to flow as Json. Json is NEVER the default when the graph could have committed to a primitive alignment. Phase A: consumer-side type expectation graph as a fixed-point iteration with type fusion propagating constraints through underspecced l-value receivers. Phase B: auto-extern with the consumer's expected type. Phase C: name resolution with Str-coercion fallback for unresolved identifiers (warning in non-strict, hard error in strict per SRD-15). Eliminates the `→ Ext` boundary-adapter warning class for non-genuine Ext cases; YAML array / object params land as the correct primitive vector type when the graph can determine it; the `set: { mode: outer }` workload-author pattern works without quoting gymnastics. |
| 80 | [Node Function Macro Collapse](80_node_function_macro_collapse.md) *(SUPERSEDED for design questions — see 80b)* | Historical scoping doc + record of shipped PRs B.1-B.15. Open design questions resolved by SRD-80b. Reference for the per-PR migration history; new design and architectural work is in SRD-80b. |
| 80b | [Macro as Universal Authoring Path](80b_macro_universal_authoring.md) *(COMMITTED — Wave 2 architecture)* | The committed architectural answer for `#[polydat_node]`: macro is the **sole** authoring path, no hand-written `PolydatNode` impls in the library. One trait (`Wire`) bridges Rust types to runtime `Value` (semantically aligned with `serde_json::Value`); `PortType`/`SlotType`/`JitType` are macro-internal projections, not operator-facing. Every shape graph fusion needs is reified by the macro via `NodeRegistration`. New wire type = one `impl Wire for X` block; new combinator (Option/paired-variadic/Ext) = one impl + one macro recognition arm; new fusion capability = one `NodeRegistration` field. Library scales to arbitrary node count; macro surface plateaus as shape diversity converges. Includes the canonical-form authoring-pattern table (every supported shape), the 7-phase migration plan from the current state to the full universal-set architecture, and the test strategy that gates each phase. |
| 84 | [Grammar-Safe Matter & Boolean Operators](84_grammar_safe_matter.md) *(DRAFT)* | Move synthesized matter off raw `BodyFragment::PolydatSource(String)` onto a grammar-safe (AST) form (Part 2, SRD-67); a caller-native expression-stub API **generic over `T: Wire`** (SRD-80b) so call sites bind the return type at compile time, with a truthy/falsy default for indeterminate predicates (Part 3); `&&`/`||` as **eager** truthiness combinators at lowest precedence (Part 1 — short-circuit deferred, needs conditional-pull); and a uniform `<expr> as <type>` **alignment-only, idempotent** type-fusion cast into the SRD-79 layer (Part 1b). Comparisons yield `U64` 1/0 (truthiness), not `Bool`. Synthesizers (metrics, poll, scope, SRD-83 stop conditions) re-base off string concatenation. Motivated by SRD-83 predicates |

### 3. Workload Specification (nmbrs-workload)

| # | Document | Scope |
|---|----------|-------|
| 25 | [Workload — Contract & Axioms](25_workload_contract.md) | **Front door** for nmbrs-workload: public surface, the W-axioms (parse-to-ParsedOp only, never-ignore-silently, standalone, extends merge-once), module map |
| 18 | [Control Flow](18_control_flow.md) | Scenario tree, scopes, iteration shapes, loop counters, scenario-tree `bindings:` + `set:` sugar (scope-local lexical-shadow primitive) |
| 18b | [Scenario Tree and Scheduler](18b_scenario_tree_and_scheduler.md) | Two-tree model, comprehension AST, kernel composition, find-by-comprehension lookup |
| 18c | [Comprehension Syntax](18c_comprehension_syntax.md) | Layered grammar — literal lists, ranges, generators, `where` filter, SI suffixes, tuple LHS, sequencer-style LUT expansions |
| 18d | [Comprehension Traversal Order](18d_comprehension_traversal_order.md) | Tuple emission order — lex, diagonal, extrema-first, concentric shells, low-discrepancy (Halton/Sobol/LHS), custom |
| 18e | [Comprehension Canonical Reference](18e_comprehension_canonical_reference.md) | **REDIRECT STUB** — superseded 2026-05-28 by `polydat/docs/design/comprehension_forms.md` (the polydat comprehension spec). Stub carries a section map pointing each former topic to its current home in the polydat spec. |
| 20 | [Workload Model](20_workload_model.md) | YAML structure, ParsedOp, blocks, tags, normalization |
| 21 | [Parameters and Bind Points](21_parameters.md) | Param resolution, bind point syntax, workload/CLI/env scoping, **params-kernel as the sole authority for workload-param values** (chain-wired into every scope via `extern`), composition with scenario-tree `bindings:` shadowing |
| 22 | [Op Sequencing](22_op_sequencing.md) | Stanza model, sequencer types, weighted ratios, cycle mapping |
| 23 | [Dynamic Controls](23_dynamic_controls.md) | Runtime-mutable per-component parameters (concurrency, rates, log level), confirmed-apply writes, enumerable declaration, reification as gauges |
| 24 | [Component Lookup](24_component_lookup.md) | Finding components by dimensional-label predicates — the selector grammar and lookup API used by dynamic controls, metrics selection, and scripted orchestration |

### 4. Execution Engine (nmbrs-runtime)

Start at [29 Execution Engine — Contract & Axioms](29_execution_engine.md): the
front door carrying the public surface, the load-bearing axioms (one walker, no-blocking-
in-async, uniform execution shell, dispenser-owned context, …), and the module map. The
documents below (30+) are the mechanism tier beneath it.

| # | Document | Scope |
|---|----------|-------|
| 29 | [Execution Engine — Contract & Axioms](29_execution_engine.md) | **Front door** for nmbrs-runtime: public surface (+ the ~25-module internal narrowing target), the 8 engine axioms, and the SRD↔module map. Pillars 1+2 for the engine |
| 30 | [Adapter Interface](30_adapter_interface.md) | DriverAdapter/OpDispenser contract, ResolvedFields, ResultBody |
| 31 | [Op Execution Pipeline](31_op_pipeline.md) | Resolve → wrap → execute → metrics flow, stanza concurrency |
| 32 | [Dispenser Wrappers](32_wrappers.md) | TraversingDispenser, ValidatingDispenser, composition order |
| 32a | [Op Wrapper Registry, Field Ownership, and Stacking Order](32a_wrapper_registry.md) *(DESIGN — not yet implemented)* | Refines SRD-32: every wrapper has a stable name, exclusive owned op-template fields, numeric stack rank, trigger predicate; default order matches today's hand-rolled cascade; workload-level (`wrappers: { order: [...] }`) and CLI (`--wrap-order`) overrides; parse-time validation of field ownership (catches misplaced `poll_interval_ms` etc.); compatibility constraints (`metrics` outermost, `traverse` innermost, `if` outside `poll` by default); `nmbrs describe wrappers` / `nmbrs describe op` for discoverability |
| 33 | [Result Validation](33_result_validation.md) | Assertions, relevancy metrics, ground truth, binding visibility |
| 34 | [Capture Points](34_capture_points.md) | Inter-op data flow, Polydat ports, capture extraction |
| 35 | [Driver Resource Lifecycle and Sharing](35_driver_resources.md) *(DESIGN — Push A/B implemented)* | Two-layer split (shell vs instance); `ResourceKey` value-equality identity; instance-shaping vs shell-shaping param partition; `ShareCapability` (driver-declared, planning-time) + `ResourceSharePolicy` (user-elevatable); paired live-instance trait methods `can_share()` (capability: thread-safe + designed for sharing) and `can_support_more_load()` (live capacity: can the instance take another caller right now? `true` = yes, route here; `false` = saturated, spawn a sibling) — driver decides the criterion, no canonical shape imposed; pool-level guard catches `quiescent-decline` driver bugs (saturation reported at zero load); pre-map-driven multi-generation refcount lifecycle, explicit async `close()` with bounded teardown, debug `resource.{attach,init,share.spawn,detach,close}` event surface with stable `generation` field; CQL adapter is the prototype consumer |
| 68 | [Dispenser-Owned Polydat Context and Single-Surface Resolution](68_dispenser_owned_polydat_context.md) *(DESIGN — Push 1+ in flight)* | Dispenser owns its canonical Polydat kernel; one resolution surface per dispenser; narrow `WireSource` trait walls adapter code off from kernel internals; `map_op` takes a `SubcontextBuilder` and uses SRD-67 two-phase materialisation; per-fiber kernel fan-out via `build_subscope` from canonical kernels (no `op_template_kernels` LUT); workload-load pre-flight is non-mutating; CQL prepared compilation is dispenser-init-time work using the canonical kernel via `WireSource`; collapses several existing parallel structures (`OpBuilder::op_template_kernels`/`op_template_programs`, `synthesis::substitute_bind_points*`, `resolve_placeholders_via_kernel`'s mutation half) into the standard subcontext mechanism |
| 71 | [Cursor Partitioning and the `cursor` Parameter](71_cursor_partitions.md) *(SHIPPED end-to-end — P1/P2/P3)* | One operator surface for projecting a cursor's domain into contiguous sub-ranges: CLI quote elision (`'cursor=0..53%'` and friends all parse the same), spec parser supporting percentages / fractions / literal ordinals in mixed form, partition lists with the tail tokens `*` (remainder), `...` (repeat last delta until used up — `90%,1%,...`), `*/N` (remainder divided into N — `90%,*/10`), and `*/recipe` (remainder shaped by recipe weights — `90%,*/fib:5`), entry modifiers `xN` (finite repetition — `90%,1%x10`) and `~` (gaps — `10%,~80%,10%` skips the middle), windowed chunking (`linear:5 in 25%..75%` — chunking resolves window-relative), a trailing iteration-order keyword (`unchanged`/`smallest_first`/`largest_first`/`random` — cardinality-keyed sorts named for their axis, spec-seeded deterministic shuffle; `unchanged`/`random` shared with SRD-18c comprehension traversal orders, bare `ascending`/`descending` rejected as position-vs-size ambiguous), pre-baked ratio recipes (`fib:7`, `bin:5`, `mul:R`, `geom`, `zipf`, `pareto`, `linear`, `ratios:…`, etc.). One boundary rule everywhere: boundaries are rounded exact cumulative positions, so rounding slack distributes instead of dropping ordinals. Polydat value types `PartitionSpec` / `Partition` / `PartitionList` plus stdlib functions (`cardinality`, `start_of`, `end_of`, `idx_of`, `mod_in`, `at`, `clamp_in`, `random_in`, `subdivide`, `partitions`) so index-arithmetic stays inside the active range; the numeric point generator formerly named `subdivide` is now `linear_starts` (SRD 18c). Explicit `over <name>` clause on cursor declarations names the partition source (param / literal / iter-var / cross-cursor) — no implicit ambient narrowing; direct consumption of a multi-partition spec is a startup error. Phase-scoped CLI overrides (`*_query.cursor=…`, exact-beats-glob, ambiguous-glob fatal, never-matching fatal), scalar `<wire>.cursor.*` dotted projections (typed slots; work in bindings and `{...}` interpolation), partition-bound open-extent cursors (`until_* over p`: base-sized chunks within the partition, partition end = hard cap, spec strings rejected for open extents), `subdivide(outer, n)` as nested comprehension source, and the `partition i/n [lo..hi)` status banner are all shipped; guide at docs/guide/cursor_partitions.md. Renames the parameter from `limit` to `cursor` to avoid collision with SQL/CQL `LIMIT` |
| 72 | [Workload `extends:`](72_workload_extends.md) *(SHIPPED — audited 2026-06-10; file-level test matrix partially pinned)* | Single-parent workload composition: top-level `extends: <relative-path>` directive, per-field merge rules (params/tags per-key, bindings concat, status_metrics union, report/scenarios/phases per-name whole-entry replace), chain semantics with cycle detection, validation runs once on merged result. Enables sibling diagnostic / sweep workloads that extend a production parent without duplication. |
| 85 | [Bundled Workloads](85_bundled_workloads.md) *(SHIPPED 2026-06-11 — P1/P2/P3)* | Embed curated + example workloads into the artifact (stdlib `include_str!` precedent), namespaced catalog names (`cql/keyvalue`, `examples/lfsr`) with local-first resolution and an ambiguity error (never silent shadowing). **Visibility tiers**: curated tier listed by default; examples tier bundled and runnable (artifact smoke-testing anywhere) but unlisted without `--all`. Discovery via `nmbrs describe workloads [name\|examples\|--all]` on the existing describe surface; required `description:` model field for curated entries (build-lint enforced); `nmbrs copy <name>` materialization with provenance header. Adapter bundles ride adapter feature gates so the catalog is truthful per binary. `extends:` resolves local-first then catalog (namespace-relative for bundled siblings) — local children extend bundled parents by name. Curated set ships `selfcheck` + `capacity_probe` + the `cql/` suite. nb5 heritage: `--list-workloads` / `--copy` |
| 86 | [Optimization](86_optimization.md) *(DRAFT — library + seam shipped 2026-06-16)* | The optimizer seam over the coordinate stream: a pluggable `Optimizer` component of the phase-execution driver, **default `sweep`** (identity Cartesian sweep — installing the seam is a no-op). Standalone, runtime-free `nmbrs-optimizers` library (9 optimizers: `sweep`, `cost_greedy_traversal`, `centroid_variant`, `nelder_mead`, `hooke_jeeves`, `bobyqa`, `cmaes`, `bayes_opt`, `hyperband`) maximizing an abstract `Objective`; the runtime `PolydatObjective` seam binds a coordinate into a phase kernel and pulls the objective wire (the SRD-40b synthetic-metric evaluation). Axis **changeover classes** (control / coordinate / fixture) unify the three realizations; nesting → minimal re-stack via refine `instance_hash`; **dual access** (SearchSpace schema vs coordinate stream); per-stop dispositions (forces SRD-83 step 4); changeover economy. Algorithm state is Rust, declarative surfaces polydat |

### 5. Metrics and Observability (nmbrs-metrics)

| # | Document | Scope |
|---|----------|-------|
| 39 | [Metrics — Contract & Axioms](39_metrics_contract.md) | **Front door** for nmbrs-metrics: public surface, the M-axioms (lock-free reporter, nanos-internal, component-tree-as-index, immutable frames, controls), module map |
| 08 | [MetricsQL — Contract & Axioms](08_metricsql.md) | **Front door** for nmbrs-metricsql (L3): the query language atop the metric query API (SRD-40c). parse/eval API over `MetricAccess`; the four `metricsql_*` polydat reader nodes (result-type affinity → Json / F64 / VecF64); Q-axioms (query-not-storage, access-API affinity, streaming≡batch) |
| 40 | [Metrics Framework](40_metrics.md) | Instruments, frames, delta semantics, reporters, scheduling |
| 41 | [Logging and Diagnostics](41_logging.md) | Conventions, Polydat compiler events, --explain mode |
| 42 | [Windowed Metrics Access](42_windowed_metrics.md) | User-specified cadences, auto-intermediate buckets, non-draining `now`, arbitrary past-duration queries |
| 44 | [Workload Checkpointing](44_workload_checkpointing.md) | Phase-boundary + cursor-state resume, per-phase identity hashing, durability ordering, invocation-agnostic error handling |
| 44a | [Checkpoint Persistence: JSONL Event Log](44a_checkpoint_jsonl.md) *(DESIGN — not yet implemented)* | Refines SRD-44 storage: append-only JSONL replaces whole-document JSON rewrites; typed-event surface (`type` discriminator) covers `session_start` / `session_end` / `phase_declared` / `scope_enter` / `scope_exit` / `phase_started` / `phase_progress` / `phase_completed` / `phase_failed`; resume planner folds the stream; truncated-tail crash recovery; future-additive event types (metric_sample / error_record / control_change) |
| 45 | [Sessions](45_sessions.md) | Session id + directory resolution, `SESSION_DIRECTORY` env / `--session-dir`, reuse policy (`error`/`restart`/`resume`), lifecycle cleanup (`--session-keep`, `--session-shelflife`), resume hint on exit |
| 47 | [MetricsQL Streaming Aggregation](47_metricsql_streaming.md) | `Reducer` algebra (distributive / algebraic / holistic), `StreamingPlan` compiler, ingest + snapshot data path, equivalence property test, holistic-function and sliding-window deferred decisions |
| 48 | [MetricsQL Continuous-Query Runtime](48_metricsql_continuous_query.md) | Plan registry, sample feed model (pull / push / watchable), actor + ArcSwap concurrency, lifecycle (register / tick / reset / unregister), window framing policy (tumbling / grid), TUI / web binding model, memory bounds |
| 93 | [Metric Instance Scope Lifecycle & Read-Path Tiering](93_metric_scope_lifecycle.md) *(DESIGN — approved for implementation)* | Completes the naming scaffold with a temporal dimension: append-only `instance_scope_event` table (enter/exit per instance per execution, written by the durable sink at the flush boundary, O(instance-lifetime) never O(pulse)); typed `CloseReason` on sealed cadence windows replacing `partial`-flag inference (quiesce ≠ exit); dual-clock stamping (`at_utc_nanos` + `at_session_nanos`) anchored by a durable `session_epoch_utc_nanos`; `metrics show` (scaffold-only) / `metrics summarize` (opt-in stats, one-pass aggregation) CLI split with dynamic completion; `mode=ro` CLI opens; tiered last-sample semantics; M7 signal contract for detached sessions (SIGTERM→ladder, SIGHUP→headless-continue, SIGQUIT→diag dump, dedicated signal thread so the stop door survives a wedged runtime). Born from the 76-minute `metrics show` incident (unindexed live db × per-instance scans × double render) and the same-day undeliverable-Ctrl-C shutdown incident |
| 106 | [Suite Traversal Provenance & Sticky Sessions](106_traversal_provenance_sticky_sessions.md) *(IMPLEMENTED)* | Provenance classes (`checkpoint: idempotent` prereqs / measurements / destructive), the composed provenance hash as THE skip-validity anchor, prereq-preserving `phases=` filtering, the suite `traverse` scenario + table namespaces, `stick_session` re-attach rung + `--session new` + the `session_notice` first-event banner |
| 107 | [Param-Scoped Provenance](107_param_scoped_provenance.md) *(IMPLEMENTED)* | Per-phase consumed-params skip validity: `base_hash` (chain excluding the params module + config digest) + derived consumed-name map with per-param value digests; polydat-owned derivation (`owned_extern_closure` / `resolve_externs_through` over the ONE multi-word node inventory); three-way gates with blockers that name the changed param |
| 108 | [Blueprint/Implementation Op Composition](108_op_binding_composition.md) *(IMPLEMENTED)* | Two load-time forms binding blueprints (protocol-agnostic scaffold workloads) to protocol op implementations: tag-contract selection (completes SRD-20's phase `tags:` selectors — parse-time resolution, load errors for empty matches) and typed `abstract:`/`implements:` slot binding (needs/yields interfaces, total two-way coverage, collision-as-error; type proof at pre-map synthesis). Execution-tree resolution and kernel composition unchanged |
| 109 | [Web-Only Client Drivers on the HTTP Module](109_web_client_drivers.md) *(IMPLEMENTED)* | Vendor client drivers as SRD-108 implementation modules of literal http op templates (zero Rust per vendor; op authenticity: the request IS the native usage), plus a driver manifest (`driver=<name>` → adapter + library + defaults, local-first then bundled, `describe drivers`) and the `results:` interface — the third typed leg (name → type in `abstract:`, delivered via ordinary `result:` bindings with SRD-70 `[*]` column projection; declared-type slot at synthesis, validator reads the projection). Auth = blueprint `connect` phase + captured token wire. gRPC/custom-binary stay real adapters |
| 113 | [Structural Reporting](113_structural_reporting.md) *(DESIGN)* | The workload's structure IS the report definition: aggregate-qualified `key_metrics` designations on phases + `anchor` cues on scenario nodes synthesize an SRD-46 report section (affine to hand-written, dumpable, suppressed by an explicit `report:` block). Rows = execution-graph nodes via SRD-77 labels; SRD-91 instruments carry contract-level spine defaults; undeclared aggregates and silent cardinality flattens are report-time errors. Gauge LWW (SRD-42 amendment) is the storage prerequisite |

### 6. Adapters

| # | Document | Scope |
|---|----------|-------|
| 50 | [CQL Adapter](50_cql_adapter.md) | Statement modes, CqlResultBody, prepared/raw dispatch, vector workloads |
| 51 | [HTTP Adapter](51_http_adapter.md) | Request templates, method/URL/body mapping |
| 52 | [Stdout and Model Adapters](52_stdout_model.md) | Format modes, field rendering, diagnostic output |
| 53 | [Vector Data Integration](53_vectordata.md) | Dataset nodes, catalog resolution, caching, metadata/predicates |

### 7. CLI and Build

| # | Document | Scope |
|---|----------|-------|
| 59 | [Terminal UI — Contract & Axioms](59_tui_contract.md) | **Front door** for nmbrs-tui: public surface (+ 8-module narrowing target), T-axioms (no-screen-buffer-for-unseen-state, display-as-fold, actor+ArcSwap, console-belongs-to-adapter) |
| 54 | [Web UI — Contract & Axioms](54_web_ui.md) | **Front door** for nmbrs-web: server/ws surface, WB-axioms (server-rendered no-SPA, read-side projection, confirmed-apply mutations); folds `internals/32` in Part 4 |
| 60 | [CLI Structure](60_cli.md) | Command tree, completions, workload discovery, bench command |
| 61 | [Single Binary, Feature-Gated Drivers](61_single_binary.md) | nmbrs binary, Cargo features, adapter selection, future drivers |
| 62 | [TUI Layout](62_tui_layout.md) | Tree-centric layout, per-phase detail blocks, dynamic Focus LOD, 120-col baseline |
| 63 | [Status Readout Templates](63_status_readouts.md) *(SHIPPED — Explanation overlay pending)* | Component-based template engine for status / summary lines, pre-baked render-step lists, compactness levels, layout ↔ content separation |
| 64 | [Report CLI](64_report_cli.md) *(DRAFT)* | `nmbrs report` command family, dynamic completion with full SRD-46 grammar parity, scratch-rendering against active session, `--add`/`--contextual`/`--replace` promotion to workload YAML |
| 49 | [MetricsQL Supported Scope](49_metricsql_supported_scope.md) | Canonical reference for what nmbrs supports as MetricsQL: parser corpus, evaluator dispatch, and tooling registry — with drift-detection tests pinning the link. Covers the [`MetricCatalog`] trait + OpenMetrics types + supported aggregate / rollup / binary-op surface |
| 40a | [Metrics Data Model](40a_metrics_model.md) | Consolidated mechanical reference: entity-relationship model + types + naming + ABNF + lookup conventions across `nmbrs-metrics::snapshot`, OpenMetrics 1.0, MetricsQL selectors, and the SQLite schema. Pins identity rules, value-type representations, label/name character grammars, and round-trip invariants. Includes a §8 gap audit against the model |
| 40b | [Synthetic Metrics from GK](40b_synthetic_metrics_from_polydat.md) *(SKETCH — not yet implemented; depends on SRD-13d)* | Cross-cutting mechanism for op templates declaring `metrics:` — formula-driven synthetic metric families published per-cycle through the standard metrics pipeline. Schema (full mapping + bare-string + list-with-wire-expression sugar). Value is always GK; **`result:` declaration** exposes capture / return-body fields as Polydat named wires so result-derived metrics share the same one-path evaluation. Dispenser-as-component (with `op` label) owns the instrument; duplicate `(family, label-cell)` collisions surface against the component's instrument set at init. `format:` is a generation-time numeric sanitiser (Excel hash patterns). `unit:` flows into both the family-name suffix and the `metric_family.unit` column. **`scope_close` cadence-streamer flush signal** (§11) so short-phase metrics never get lost between cadence pulses — generic, applies to every component. Op-template Polydat scope layering + elision live in SRD-13d. Workload-specific demo plan: [`docs/SRD/notes/synthetic_metrics_cql_vector_demo.md`](notes/synthetic_metrics_cql_vector_demo.md) |
| 40c | [The Metric Query API](40c_metric_query_api.md) | **Front door** for `nmbrs-metrics::queryapi` — the data-access *service* the metric readers pull through. `MetricAccess` fetch contract (`select_range` / `select_instant`, 1:1 with a MetricsQL selector); `Vector` / `Series` / `Sample` / `Matcher` shapes (f64); `MetricCatalog` enumeration; live + sqlite backends; `AccessProvider` inventory locator + `install_live_access` / `live_access`. MQ-axioms incl. **reader-node volatility** (`Purity::Nondeterministic`). Distinct from the metrics cadence feed (SRD-42): fetch/enumerate surface vs. pulsed publish/subscribe |

---

## SRDv1 → SRDv2 Mapping

| SRDv2 | Source SRDs (v1) |
|-------|-----------------|
| 01 System Overview | 01, 20, 37, 39 |
| 02 Concurrency | 21, 40 (concurrency section) |
| 03 Error Handling | 41 |
| 10 Polydat Language | 05, 06, 07, 14, 24 |
| 11 Polydat Evaluation | 02, 10, 12, 13, 26, 44 |
| 12 Polydat Stdlib | 03, 04, 08, 09, 11, 25, 30 |
| 13 Polydat Modules | 27, 36, 44, 45 |
| 14 Polydat Config | 48 |
| 20 Workload Model | 17, 22, 35 |
| 21 Parameters | 42 |
| 22 Op Sequencing | 22 |
| 30 Adapter Interface | 38 |
| 31 Op Pipeline | 33, 40 |
| 32 Wrappers | 33, 34, 47 (wrapper section) |
| 33 Result Validation | 47 |
| 34 Capture Points | 28 |
| 40 Metrics | 15, 16 |
| 41 Logging | 43, 45 |
| 50 CQL Adapter | 46, 50 (from code) |
| 51 HTTP Adapter | (from code) |
| 52 Stdout/Model | 29 |
| 53 Vectordata | 46, (vectordata nodes) |
| 60 CLI | 23, 32, 35 |
| 61 Single Binary | 37 |

## Known Tensions — resolved

The v1 SRDs flagged seven cross-cutting tensions that v2 had
to answer. All seven have now been folded into the relevant
SRD sections. The pointers below exist so anyone arriving
from a v1 reference can jump to the current authoritative
home of each decision.

| # | v1 tension | Current home |
|---|------------|--------------|
| 1 | Binding visibility scope | SRD 10 §"GK as the unified access surface" (GK owns all runtime-value resolution; no separate "extras" pathway). |
| 2 | `{polydat:name}` qualifier for Polydat constants | SRD 10 §"GK as the unified access surface" (subsumed: every name resolves through the Polydat graph, no separate qualifier needed). |
| 3 | Per-phase config override | SRD 21 §"Parameter Resolution" (block-level `params:`, closest-wins; Polydat helpers `this_or` / `required` / predicates for explicit layering). |
| 4 | `cycles=train_count` resolution | SRD 10 §"GK as the unified access surface" + SRD 21 §"Explicit layering with Polydat helpers". `cycles` is not special; cursors are arbitrary names; `train_count` is a GK-folded constant reified into the local or workload scope. |
| 5 | Adapter vs core op-field boundary | SRD 30 §"Core-first field processing" (core consumes its fields first; adapters see only their own fields; unknown fields are errors). |
| 6 | `input cycle: u64` boilerplate | SRD 10 §"Input Declaration" (inputs inferred when the declaration is omitted; `cycle` is not a magic identifier). |
| 7 | Result extraction model | SRD 33 §"Result Extraction" (universal JSON access + typed accessors / traversers as an opt-in hot-path optimization). |

For outstanding unresolved items, see
[`99_open_tensions_memo.md`](99_open_tensions_memo.md) (kept
for historical reference; empty as of its resolution).
