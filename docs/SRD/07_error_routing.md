# 07: Error Routing — Contract & Axioms (nmbrs-errorhandler)

Front door for **nmbrs-errorhandler** (layer L0, standalone): the reusable error-routing
primitive — pattern → handler-chain. Another **tight-contract** exemplar. Pillars 1+2 of
the [Subsystem Treatment Standard](00b_subsystem_standard.md).

## Contract

**Surface and edges:** authoritative in [SRD 05 §Contract Registry](05_dependency_rules.md).
In brief — exports `ErrorDetail` / `ErrorHandler` (trait) / `ErrorRouter` (re-exported) + the
`handlers` module (`StopHandler` / `WarnHandler` / `RetryHandler` / `CounterHandler` / …);
zero internal dependencies (standalone).

## Axioms

- **E1 — Every error is routed, never silently dropped.** An unmatched error is a routing
  gap to fix, not a discard. [SRD 03 §Silent-failure policy].
- **E2 — Router = ordered pattern → handler-chain.** First match wins within a scope;
  handlers compose (count, warn, retry, stop). [SRD 03].
- **E3 — Standalone primitive.** The router knows nothing about activities. The engine
  composes it (`nmbrs-runtime::error_policy`); how error *handling* stays orthogonal to
  *stop conditions* is [SRD 82](82_uniform_execution_shells.md)/[SRD 83](83_stop_conditions.md).

## Mechanism (Pillar 3)
[SRD 03](03_error_handling.md) (scoping, retry, silent-failure policy); composition into
the execution shells is [SRD 82](82_uniform_execution_shells.md) / [SRD 83](83_stop_conditions.md).

## See also
`crates/nmbrs-errorhandler/src/lib.rs`; [SRD 03](03_error_handling.md); [SRD 82](82_uniform_execution_shells.md); [SRD 83](83_stop_conditions.md).
