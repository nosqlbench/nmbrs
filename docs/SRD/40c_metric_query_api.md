# 40c: The Metric Query API (nmbrs-metrics::queryapi)

Front door for the **metric query API** — the data-access *service* boundary inside
`nmbrs-metrics` (layer L2). `nmbrs-metrics` is the foundational data-access library; the
`queryapi` module exposes its query surface as a service that any reader — the MetricsQL
engine ([SRD 08](08_metricsql.md)), the `metricsql_*` polydat nodes, report/plot CLIs —
locates and reads through. It is the "metric-reader surface" of
[SRD 86 §10](86_optimization.md). Pillars 1+2 of the
[Subsystem Treatment Standard](00b_subsystem_standard.md).

Distinct from the **metrics cadence feed** ([SRD 42](42_windowed_metrics.md)): the cadence
feed *publishes* windows on a pulse; the query API is the *fetch / enumerate* surface a
reader pulls through. The live backend here performs a **cadence read** of the feed's store
via `MetricsQuery`.

## Contract

**Surface** (`nmbrs_metrics::queryapi`):

- **Access contract** — `trait MetricAccess: Send + Sync`:
  `select_range(matchers, start_ms, end_ms) -> Result<Vector, QueryError>` (required);
  `select_instant(matchers, at_ms, lookback) -> Result<Vector, QueryError>` (default =
  range + reduce-to-latest-per-series). Signatures map **1:1 onto a MetricsQL parsed
  selector**: a bare / instant selector → `select_instant`; a range selector `m[w]` →
  `select_range`.
- **Result shapes** — `Vector(Vec<Series>)`; `Series { labels, samples }`;
  `Sample { timestamp_ms: i64, value: f64 }`; selector `Matcher { label, op, value }` +
  `MatchOp { Eq, Ne, EqRegex, NeRegex }`. f64 throughout (the engine computes in f64).
- **Enumeration** — `trait MetricCatalog` (metric families / label keys / label values /
  series) + `CachedCatalog`: the small, slow-changing sibling of fetch, consumed by name
  completion. Plus `MetricFamilyMeta`, `MetricType`, `ExemplarPoint`, `LabelSet`.
- **Backends** — `MetricsQueryAccess` (live in-process; wraps the session `MetricsQuery`,
  i.e. reads the cadence-feed store) and `SqliteDataSource` (feature `sqlite`; reads a
  session `metrics.db`).
- **Service location** — `install_live_access(Arc<dyn MetricAccess>)` / `live_access()` for
  the per-session live service; `AccessProvider { scheme, open }` + `provider(scheme)` — an
  `inventory`-registered locator so file / external backends (sqlite) are reachable by
  scheme without the reader depending on the backend's crate or features.

## Axioms

- **MQ1 — Access, not aggregation.** This surface fetches and enumerates only. Aggregation,
  rollups, and arithmetic — the query *language* — stay in the MetricsQL engine
  ([SRD 08](08_metricsql.md)), layered over this contract. Keeping the contract thin is what
  lets the engine sit on any data service.
- **MQ2 — Shapes are owned here.** `Vector` / `Series` / `Sample` / `Matcher` are the
  canonical result shapes; consumers (metricsql) re-export them rather than re-model them,
  so a selector fetch is a 1:1 call. No schema is duplicated in the query layer.
- **MQ3 — Locate, don't bind.** A reader locates a service at runtime — the live service via
  `live_access()` (the session `MetricsQuery` is not static, so the runner installs it per
  session), file / external services via `provider(scheme)` (inventory). The reader never
  depends on a concrete backend.
- **MQ4 — Metrics-reading polydat nodes are volatile.** Every polydat node that reads through
  this surface (`metric`, `metric_window`, and the four `metricsql_*` nodes) MUST declare
  `Purity::Nondeterministic`. A zero-input `Pure` reader would be const-folded at compile
  time against the empty reporter (→ 0) and cached forever; `Nondeterministic` marks it
  `Dynamic` (polydat R1.v) so it is never folded and re-evaluates on every pull.

## Mechanism (Pillar 3)
`crates/nmbrs-metrics/src/queryapi/{mod,shapes,live,sqlite,catalog}.rs`. The MetricsQL engine
consumes the surface through `nmbrs_metricsql::eval::EvalContext.data: &dyn MetricAccess`
([SRD 08](08_metricsql.md)); the `metricsql_*` nodes project a fetched `Vector` to a polydat
`Value` by result-type affinity ([SRD 08 §Reader nodes](08_metricsql.md)).

## See also
`crates/nmbrs-metrics/src/queryapi/mod.rs`; [SRD 08](08_metricsql.md) (the query language atop it);
[SRD 42](42_windowed_metrics.md) (the cadence feed the live backend reads);
[SRD 49](49_metricsql_supported_scope.md); [SRD 86 §10](86_optimization.md).
