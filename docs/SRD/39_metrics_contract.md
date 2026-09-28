# 39: Metrics & Observability — Contract & Axioms (nmbrs-metrics)

Front door for **nmbrs-metrics** (layer L2): the component tree, instruments, cadence
reporter, and the read-side query surface. A well-factored crate — almost every module is
a deliberate public surface. Pillars 1+2 of the
[Subsystem Treatment Standard](00b_subsystem_standard.md).

## Contract

**Surface, inbound contract, and allowed edges:** authoritative in
[SRD 05 §Contract Registry](05_dependency_rules.md). In brief — the `component` tree +
`instruments` + the lock-free `cadence_reporter` + the read-side `metrics_query` + the
`queryapi` data-access service ([SRD 40c](40c_metric_query_api.md)) +
`reporters` (SQLite / VictoriaMetrics), plus `labels`/`selector`/`controls`/`snapshot`/
`cadence`/`scheduler`/`summaries`/`diag`/`polydat_nodes`; consumes `polydat` (`ast`,
`kernel`) for metric-reading nodes; `validation` is internal.

## Axioms

- **M1 — Lock-free reporter.** `CadenceReporter` uses an **actor + `ArcSwap`**, never a
  `RwLock`. Preserve this in any change.
- **M2 — Nanoseconds internal.** All internal time values are tracked as nanoseconds.
- **M3 — The component tree is the canonical metric index.** Instruments hang on
  components; lookup is by dimensional-label selector, never a sidecar map. [SRD 24](24_component_lookup.md).
- **M4 — Immutable frame snapshots.** Metrics are captured as immutable snapshots at
  cadence intervals (delta semantics); readers never see torn state. [SRD 40](40_metrics.md).
- **M5 — Controls are reified gauges + a confirmed-apply write path.** Dynamic controls
  are enumerable on the tree and applied transactionally. [SRD 23](23_dynamic_controls.md).

## Mechanism (Pillar 3)
[SRD 40](40_metrics.md) (framework), [SRD 40a](40a_metrics_model.md) (data model),
[SRD 40b](40b_synthetic_metrics_from_polydat.md) (synthetic metrics), [SRD 40c](40c_metric_query_api.md)
(the metric query API / data-access service), [SRD 42](42_windowed_metrics.md)
(windowed access — the metrics cadence feed), [SRD 24](24_component_lookup.md) (lookup). The
MetricsQL query language is its own crate atop the query API — [SRD 08](08_metricsql.md).

## See also
`crates/nmbrs-metrics/src/lib.rs`; [SRD 40](40_metrics.md); [SRD 40a](40a_metrics_model.md); [SRD 42](42_windowed_metrics.md).
