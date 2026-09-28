# 25: Workload Specification — Contract & Axioms (nmbrs-workload)

Front door for **nmbrs-workload** (layer L2): the YAML → typed-model parser. It turns a
workload document into `ParsedOp`s, bind points, tags, and the scenario/phase structure —
and contains **no execution logic**. Pillars 1+2 of the
[Subsystem Treatment Standard](00b_subsystem_standard.md).

## Contract

**Surface, inbound contract, and allowed edges:** authoritative in
[SRD 05 §Contract Registry](05_dependency_rules.md). In brief — exports `model`
(`Workload` / `WorkloadPhase` / `ParsedOp`), `parse`, `bindpoints`, `tags`, `inline`,
`catalog`, `edit`, `extends`, `report`, `polydat_matter` (the workload↔polydat bridge);
consumes `polydat` (`ast`, `dsl`); `template` / `spectest` are internal.

## Axioms

- **W1 — Parse-to-`ParsedOp` is the only bridge to execution.** The crate produces a typed
  model; it never runs anything. The Workload→Activity contract is `ParsedOp`. [SRD 20].
- **W2 — Never ignore silently.** Every field is acted on or rejected; an unknown key is an
  error, not a discard. [SRD 15 strict mode].
- **W3 — Standalone parser.** Depends only on `polydat`; independently usable. [SRD 01].
- **W4 — `extends` is single-parent, merge-then-validate-once.** Per-field merge rules,
  cycle detection, and a single validation pass on the merged result. [SRD 72].
- **W5 — Bundled catalog is artifact-embedded and local-first.** Curated + example
  workloads resolve local-first with an ambiguity error (never silent shadowing). [SRD 85].

## Mechanism (Pillar 3)
[SRD 18](18_control_flow.md) (control flow), [SRD 20](20_workload_model.md) (model),
[SRD 21](21_parameters.md) (parameters), [SRD 22](22_op_sequencing.md) (op sequencing),
[SRD 72](72_workload_extends.md) (extends), [SRD 85](85_bundled_workloads.md) (bundled).

## See also
`crates/nmbrs-workload/src/lib.rs`; [SRD 20](20_workload_model.md); [SRD 21](21_parameters.md); [SRD 72](72_workload_extends.md); [SRD 85](85_bundled_workloads.md).
