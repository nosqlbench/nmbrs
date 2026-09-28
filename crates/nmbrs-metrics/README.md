# nmbrs-metrics

Metrics collection and reporting for [nmbrs](https://github.com/nosqlbench/nmbrs).
It provides the component tree that holds instruments (counters, gauges,
HDR histograms, timers), a cadence reporter that coalesces snapshots into
time windows, reporters for several output formats, and a read-side query
API. It is written for the nmbrs runtime and its UIs; it can be used on its
own, but its design follows nmbrs's session, phase and activity model.

## Where it sits in nmbrs

`nmbrs-metrics` is the foundational metrics and data-access library of the
workspace. It is used by
[`nmbrs-runtime`](https://crates.io/crates/nmbrs-runtime),
[`nmbrs-rate`](https://crates.io/crates/nmbrs-rate),
[`nmbrs-tui`](https://crates.io/crates/nmbrs-tui),
[`nmbrs-web`](https://crates.io/crates/nmbrs-web) and the
[`nmbrs`](https://crates.io/crates/nmbrs) CLI. The MetricsQL engine,
[`nmbrs-metricsql`](https://crates.io/crates/nmbrs-metricsql), evaluates
queries over the access API defined here (`queryapi`).

End users normally install the [`nmbrs`](https://crates.io/crates/nmbrs)
CLI rather than depending on this crate directly.

## What it provides

- **Component tree** (`component`): hierarchical components, each with
  dimensional labels (`session=…`, `phase=…`, `activity=…`), props, an
  instrument set, and a control registry. A component's effective labels
  are its own labels plus those of its parent chain. Components are looked
  up with `selector::Selector` (`component::find`, `find_one`, `count`).
  `cells` resolves one child component per dimensional coordinate.
- **Instruments** (`instruments`): `Counter`, `ValueGauge`, `Histogram` and
  `Timer`. Histograms and timers are HDR-backed. They are recorded on the
  hot path and read as delta windows.
- **Snapshots** (`snapshot::MetricSet`): OpenMetrics-shaped captures of the
  tree at one point in time.
- **Cadences and the cadence reporter** (`cadence`, `cadence_reporter`):
  `Cadences` and `CadenceTree` plan the declared windows (for example 1s,
  10s, 1m) plus any intermediate layers; `CadenceReporter` folds snapshots
  into those windows, keeps per-cadence history, and delivers sealed
  windows to async subscribers. `scheduler` drives capture on a base
  interval.
- **MetricsQuery** (`metrics_query::MetricsQuery`): the single read API
  over the cadence store and live tree. Modes include `now`,
  `cadence_window`, `session_lifetime`, `increase_over` and
  `distribution_over`, each filtered by a `Selection`.
- **Query API** (`queryapi`): the data-access service boundary. It defines
  the `Vector` / `Series` / `Sample` result shapes, label `Matcher`s, and
  the `MetricAccess` trait (`select_range`, `select_instant`). Services are
  located at runtime: a live in-process service via
  `install_live_access` / `live_access`, and file backends registered as
  `AccessProvider`s and found by scheme with `provider`. `catalog` exposes
  metric-family enumeration (`MetricCatalog`); `hybrid::HybridStore`
  combines tiers (for example in-memory and sqlite) into one
  `MetricAccess`. The query API has no query language; aggregation lives in
  `nmbrs-metricsql`.
- **Reporters** (`reporters`): console, CSV, a JSONL metrics log,
  per-instance JSONL files, an in-memory summary report, OpenMetrics /
  Prometheus text rendering (`render_prometheus_text`) and parsing
  (`parse_prometheus_text`), plus feature-gated SQLite and VictoriaMetrics
  push reporters.
- **Controls** (`controls`): `Control<T>`, a named, typed value attached to
  a component. `Control::set` is async and completes only after every
  registered `ControlApplier` has acknowledged the change. A control can
  optionally be published as a gauge (`ControlBuilder::reify_as_gauge`).
- **Summaries** (`summaries`): retained views fed from outside the hot
  path, such as `HdrSummary`, `LiveWindowHistogram`, `BinomialSummary`
  (sparklines), `Ewma`, `F64Stats` and `PeakTracker`.
- **Polydat metric nodes** (`polydat_nodes`): registers the `metric` and
  `metric_window` nodes with [polydat](https://crates.io/crates/polydat) so
  workloads can read live metrics (`cycles`, `errors`, `rate`, `p50`,
  `p99`, `mean`). The runner installs the query with
  `polydat_nodes::set_global_query`.
- **Supporting modules**: `validation` (OpenMetrics name, label, unit and
  bucket checks), `thread_pools` (named OS thread pools with scheduling
  policy; realtime priority and affinity apply on Linux), and `diag`
  (pluggable warning/info sink).

### Example

Build a component tree, register a counter, and render the current values
as Prometheus text:

```rust
use std::collections::HashMap;
use std::sync::Arc;
use nmbrs_metrics::component::{self, Component, InstrumentRef};
use nmbrs_metrics::instruments::counter::Counter;
use nmbrs_metrics::labels::Labels;
use nmbrs_metrics::reporters::openmetrics::render_prometheus_text;
use nmbrs_metrics::selector::Selector;

let root = Component::root(Labels::of("session", "demo"), HashMap::new());

let ops = Arc::new(Counter::new(Labels::of("name", "ops")));
root.write()
    .unwrap()
    .register_instrument("ops", InstrumentRef::Counter(ops.clone()))
    .unwrap();
ops.inc_by(3);

// Effective labels include the chain back to the root.
assert_eq!(
    root.read().unwrap().effective_labels().get("session"),
    Some("demo")
);

// An empty selector matches the root on a fresh tree.
assert_eq!(component::find(&root, &Selector::new()).len(), 1);

let text = render_prometheus_text(&root.read().unwrap().capture_current());
println!("{text}");
```

## Cargo features

All features are off by default.

| Feature | Enables |
|---|---|
| `sqlite` | `reporters::sqlite::SqliteReporter` (writes the normalized session metrics database) and `queryapi::sqlite` (the sqlite read backend, registered as the `sqlite` access provider). Pulls in `rusqlite` with the bundled SQLite. |
| `victoriametrics` | `reporters::victoriametrics::VictoriaMetricsReporter`, which pushes Prometheus text to a VictoriaMetrics `/api/v1/import/prometheus` endpoint. Pulls in `reqwest` (blocking). |
| `all-reporters` | `sqlite` and `victoriametrics`. |

## Links

- Repository: <https://github.com/nosqlbench/nmbrs>
- Crate source: <https://github.com/nosqlbench/nmbrs/tree/main/nmbrs-metrics>
- API docs: <https://docs.rs/nmbrs-metrics>
- Metrics contract (SRD 39): <https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/39_metrics_contract.md>

## License

Apache-2.0
