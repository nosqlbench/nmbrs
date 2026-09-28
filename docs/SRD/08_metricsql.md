# 08: MetricsQL — Contract & Axioms (nmbrs-metricsql)

Front door for **nmbrs-metricsql** (layer L3): a Rust port of VictoriaMetrics MetricsQL —
lexer, parser, evaluator, the streaming/continuous-query runtime, and the four `metricsql_*`
polydat reader nodes. It is the **query language atop the metric query API**
([SRD 40c](40c_metric_query_api.md)): `nmbrs-metrics` owns the data-access service (fetch,
result shapes, backends); this crate parses and evaluates MetricsQL over it. Pillars 1+2 of
the [Subsystem Treatment Standard](00b_subsystem_standard.md).

## Contract

**Surface and edges:** authoritative in [SRD 05 §Contract Registry](05_dependency_rules.md).
In brief — `parser::parse` + `ast::Expr`; `eval::{evaluate, EvalContext}` evaluating over a
`MetricAccess` data source (re-exported from `nmbrs_metrics::queryapi` through `eval`);
`streaming::{StreamingPlan, …}`; `runtime` (feature `runtime`); and the four `metricsql_*`
polydat reader nodes (feature `polydat-nodes`, §Reader nodes below). The inbound contract is
a `MetricAccess` implementation supplied by the host — in nmbrs, nmbrs-metrics' live or sqlite
backend ([SRD 40c](40c_metric_query_api.md)).

Its public module set is its own library contract — broader than what the nmbrs workspace
consumes — so it is **D5-exempt** ([SRD 05](05_dependency_rules.md)). It is **not**
zero-internal-dependency, however: it depends on `nmbrs-metrics` (L3 → L2) to reach the
data-access seam. The result shapes and the `catalog`/sqlite backend that earlier lived here
now live in `nmbrs-metrics::queryapi` ([SRD 40c](40c_metric_query_api.md)); this crate
re-exports the shapes through `eval`, never re-models them.

## Axioms

- **Q1 — Query language, not storage.** This crate computes; it never owns sample storage or
  metric shapes. Those live in `nmbrs-metrics::queryapi` ([SRD 40c](40c_metric_query_api.md)).
  The data-access seam is the `MetricAccess` trait; metricsql depends on nmbrs-metrics to reach
  it. (Storage-as-a-service boundary, viewed from the consuming side.)
- **Q2 — Access-API affinity.** The shapes the evaluator consumes — `Vector` / `Series` /
  `Sample` / `Matcher` — ARE the queryapi's shapes, re-exported rather than re-modelled, so a
  selector fetch is a 1:1 call onto `MetricAccess::select_range` / `select_instant`. No schema
  is duplicated here.
- **Q3 — Streaming ≡ batch.** A `StreamingPlan` must produce the same result as batch
  evaluation; the `Reducer` algebra (distributive / algebraic / holistic) is the load-bearing
  artifact and is property-tested for equivalence. [SRD 47](47_metricsql_streaming.md).

## Reader nodes (`metricsql_*`)

Four polydat nodes (feature `polydat-nodes`) make a MetricsQL query readable from a workload's
polydat. Each parses its query once at construction, then on every pull locates a live
service (`queryapi::live_access`), evaluates through this engine, and **projects** the result
`Vector` to a polydat `Value` by **result-type affinity**:

| node | shape | output |
|---|---|---|
| `metricsql` | general (full labeled result) | `Value::Json` |
| `metricsql_scalar` | scalar (asserts 1 series × 1 sample) | `Value::F64` |
| `metricsql_vector` | instant vector | `Value::VecF64` |
| `metricsql_window` | range vector (single series) | `Value::VecF64` |

Scope/timeframe are expressed *in the MetricsQL query*, not in the function name; f64
throughout. All four declare `Purity::Nondeterministic` — they are metrics readers and must
never be const-folded ([SRD 40c](40c_metric_query_api.md) MQ4).

## Mechanism (Pillar 3)
[SRD 40c](40c_metric_query_api.md) (the metric query API it evaluates over),
[SRD 47](47_metricsql_streaming.md) (streaming aggregation),
[SRD 48](48_metricsql_continuous_query.md) (continuous-query runtime),
[SRD 49](49_metricsql_supported_scope.md) (supported scope + drift tests).

## See also
`crates/nmbrs-metricsql/src/lib.rs`; [SRD 40c](40c_metric_query_api.md); [SRD 47](47_metricsql_streaming.md); [SRD 48](48_metricsql_continuous_query.md); [SRD 49](49_metricsql_supported_scope.md).
