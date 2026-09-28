# 09: Polydat Contract Surface

The front door between this reference (`docs/SRD`, the nmbrs system) and the
**polydat** substrate. polydat is a standalone, independently extractable crate; its
*substrate design* lives in `polydat/docs/` (the axiom + mechanism tiers), and this
document names the **public contract surface** nmbrs consumes from it. It is the
realization of [Pillar 1 (Contract surface)](00b_subsystem_standard.md) for the
exemplar subsystem.

If you are looking for *how the kernel works* (grammar, scopes, engines, JIT, wire
materialization), read `polydat/docs/`. If you are looking for *what nmbrs depends on*
and *where each polydat-integration SRD's substrate lives*, read here.

---

## What nmbrs consumes from polydat

The public boundary, by import frequency across `nmbrs-runtime` / `nmbrs-workload` /
`nmbrs-metrics` / adapters (see [SRD 05 §Contract Registry](05_dependency_rules.md)):

| polydat surface | Role at the boundary |
|---|---|
| `polydat::kernel::{PolydatKernel, PolydatProgram, PolydatState}` | the compiled DAG (`Arc`-shared) + per-fiber state; the primary runtime handle |
| `polydat::ast::{Value, PolydatNode, Port, PortType}` | typed variate exchange + the node trait library code registers against |
| `polydat::dsl::compile_polydat` (and `dsl::compile`) | the DSL → kernel compilation entry point |
| `polydat::compile::assembly::{PolydatAssembler, WireRef}` | programmatic kernel construction |
| `polydat::kernel::subcontext::{SubcontextBuilder, PolydatMatter}` | the walled-off sub-scope construction protocol (SRD-67) |
| `polydat::kernel::extract_manifest` | scope/output introspection |
| `polydat::iteration::*` | comprehension / cursor iteration sources |
| `polydat::dsl::registry::*`, `polydat::dsl::events::CompileEvent` | function-registry lookup + the compiler diagnostic event stream |
| `polydat::audit` | the host-log sink bridge — the runner installs `observer::log` here |

The **boundary is enforced**: rule **D1** keeps polydat depending only on
`polydat-derive`; rule **D6** forbids any consumer reaching past this surface into a
deep internal path (`polydat::compile::jit::…`, `polydat::library::support::…`). See
[`crates/nmbrs/tests/architecture_rules.rs`](../../crates/nmbrs/tests/architecture_rules.rs).

---

## Where the substrate design lives

The polydat-integration SRDs in this directory are **stubs or reduced nmbrs-side
framing**; the authoritative substrate design is in `polydat/docs/`. Map:

| docs/SRD (nmbrs integration) | polydat/docs substrate (authoritative) |
|---|---|
| [10 Language](10_polydat_language.md) (nmbrs-side: unified access surface, reification, op-level bindings) | **`design/polydat_grammar.md`** (definitive surface-language spec+guide; `design/grammar.md` is its formal appendix), `design/language_spec.md`, `design/graph_compiler.md` |
| [11 Evaluation](11_polydat_evaluation.md) (nmbrs-side: FiberBuilder, cursor-driven eval) | `design/evaluation_model.md`, `design/runtime_model.md` |
| [12 Stdlib](12_polydat_stdlib.md) *(stub)* | `design/library_catalog.md` |
| [13 Modules](13_polydat_modules.md) (nmbrs-side: diagnostic event stream) | `design/module_system.md` |
| [13c Scope Model](13c_polydat_scope_model.md) *(stub)* | `design/scope_model.md` |
| [13f Wire Materialization](13f_cross_scope_wire_materialization.md) (nmbrs-side: synthesizer rule, true-up history) | `design/wire_materialization.md` |
| [14 Config Expressions](14_polydat_config_expressions.md) (nmbrs-side: host param resolution) | `design/expression_engine.md`, `design/grammar.md` |
| [16 Engines](16_polydat_engines.md) *(stub)* / [16b JIT](16b_polydat_jit.md) *(stub)* | `design/engines.md`, `design/jit_boundary.md` |
| [67 Subcontext Construction](67_polydat_subcontext_construction.md) *(stub)* | `design/subcontext_construction.md` |
| [74 None Propagation](74_none_propagation.md) *(stub)* | `design/none_semantics.md` |
| Comprehensions ([18b](18b_scenario_tree_and_scheduler.md)–[18f](18f_comprehension_source_forms.md)) | `design/comprehension_forms.md` |

**Axiom tier** (the load-bearing invariants the mechanism docs above delegate to):
`composition_substrate.md` (S/T/L pillars, slot contract), `grammar.md` (productions,
type inference), `graph_compiler.md` (hoisting + fusion), `runtime_model.md` (data flow,
invalidation, determinism), `expression_engine.md` (host-embeddable evaluation).

The history of this consolidation — the per-SRD move-and-reduce plan that produced the
stubs above — is the executed plan in
[polydat_srd_audit.md](../polydat_srd_audit.md), retained as the historical record.

---

## See also
- [SRD 00b — Subsystem Treatment Standard](00b_subsystem_standard.md) (polydat is the exemplar)
- [SRD 05 — Dependency Rules](05_dependency_rules.md) (the enforced boundary)
- [SRD 01 §Contract Boundaries](01_system_overview.md)
