# nmbrs-metricsql

A MetricsQL parser and evaluator in Rust. It is a port of
[VictoriaMetrics/metricsql](https://github.com/VictoriaMetrics/metricsql)
plus the parts of VictoriaMetrics' `vmselect/promql` needed for query
evaluation. nmbrs uses it for its plots, tables, reports and live metric
readouts. The parser, AST and prettifier can be used outside nmbrs; the
evaluator reads data through the `MetricAccess` trait from
[`nmbrs-metrics`](https://crates.io/crates/nmbrs-metrics), which you can
implement over your own storage.

## Where it sits in nmbrs

`nmbrs-metricsql` sits above
[`nmbrs-metrics`](https://crates.io/crates/nmbrs-metrics): the metrics crate
defines the data-access service (`queryapi`: `MetricAccess`, `Vector`,
`Series`, `Matcher`), and this crate adds the query language, aggregation,
rollups and arithmetic on top. The [`nmbrs`](https://crates.io/crates/nmbrs)
CLI depends on it with the `runtime` and `polydat-nodes` features enabled.

End users normally install the [`nmbrs`](https://crates.io/crates/nmbrs)
CLI rather than depending on this crate directly.

## What it provides

- **Parsing** (`parse`, `parse_for_prettify`, `ast::Expr`): `parse` lexes
  and parses a query into an AST, expanding `WITH` templates and folding
  constants. `parse_for_prettify` keeps `WITH` templates and unfolded
  literals so the query can be printed back as written.
- **Prettifier** (`prettifier::pretty_string`, `prettifier::pretty_print`):
  renders an AST as a canonical single-line query string, or as a
  multi-line form for display.
- **Evaluation** (`evaluate`, `evaluate_range`, `EvalContext`): evaluates an
  `Expr` against any `MetricAccess` backend over a time range, returning
  `Vec<Series>`. `evaluate_range` runs the query at each step from
  `start_ms` to `end_ms`. The evaluator covers a subset of MetricsQL
  (selectors, common aggregations, binary operators and common rollups);
  expressions outside that subset return `EvalError::NotYetImplemented`.
- **Grammar catalog** (`grammar`): the known aggregate operators and
  rollup functions, with lookup and prefix-completion helpers and a flag
  for whether each one is evaluable or parser-only.
- **Streaming aggregation** (`compile_streaming`, `StreamingPlan`,
  `streaming::ReducerKind`): compiles supported query shapes into an
  incremental plan that ingests samples and produces snapshots. Reducers
  cover `sum`, `count`, `min`, `max`, `group`, `avg`, `stddev`, `stdvar`,
  their `*_over_time` forms, `first_over_time`, `last_over_time`,
  `increase`, `delta`, `rate`, and `quantile_over_time` (HDR-histogram
  based, with bounded relative error). Streaming `rate` and `increase` use
  `(last - first)` over the window without counter-reset adjustment or
  window-edge extrapolation, so they are not guaranteed to match batch
  evaluation.
- **Query rewriting** (`query_rewrite::inject_default_exec_id`): an
  nmbrs-specific AST rewrite used to scope queries to an execution.

### Parse and print

```rust
use nmbrs_metricsql::parse;
use nmbrs_metricsql::prettifier::pretty_string;

let expr = parse(r#"sum(rate(http_requests_total{job="api"}[5m])) by (instance)"#)
    .expect("valid MetricsQL");
println!("{}", pretty_string(&expr));
```

### Evaluate against your own data

Implement `MetricAccess` (re-exported from `nmbrs-metrics`) over your
storage, then evaluate a parsed expression. Metric names are carried in
the `__name__` label.

```rust
use nmbrs_metricsql::eval::{Matcher, Sample, Series, Vector};
use nmbrs_metricsql::{evaluate, parse, DataSourceError, EvalContext, MetricAccess};

struct InMemory(Vec<Series>);

impl MetricAccess for InMemory {
    fn select_range(
        &self,
        matchers: &[Matcher],
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vector, DataSourceError> {
        let series = self
            .0
            .iter()
            .filter(|s| matchers.iter().all(|m| m.matches(&s.labels)))
            .map(|s| Series {
                labels: s.labels.clone(),
                samples: s
                    .samples
                    .iter()
                    .copied()
                    .filter(|p| p.timestamp_ms >= start_ms && p.timestamp_ms <= end_ms)
                    .collect(),
            })
            .collect();
        Ok(Vector::new(series))
    }
}

let host = |h: &str, v: f64| Series {
    labels: vec![
        ("__name__".to_string(), "cpu".to_string()),
        ("host".to_string(), h.to_string()),
    ],
    samples: vec![Sample { timestamp_ms: 1_000, value: v }],
};
let data = InMemory(vec![host("a", 1.0), host("b", 2.0)]);

let ctx = EvalContext {
    data: &data,
    start_ms: 1_000,
    end_ms: 1_000,
    step_ms: 1_000,
    lookback_ms: Some(60_000),
    query_start_ms: None,
    query_end_ms: None,
};
let result = evaluate(&ctx, &parse("sum(cpu)").unwrap()).unwrap();
assert_eq!(result.len(), 1);
assert_eq!(result[0].samples[0].value, 3.0);
```

## Upstream compatibility tests

`tests/fixtures/parser_round_trip.json` (500 cases, from upstream
`parser_test.go`) and `tests/fixtures/prettifier_round_trip.json` (51
cases, from upstream `prettifier_test.go`) are harvested from the
VictoriaMetrics/metricsql test files. `tests/parity.rs` parses each input
and checks that the prettified output equals upstream's expected string.
By default the test only loads the fixtures; set `RUN_METRICSQL_PARITY=count`
to run every case and report the pass rate, or `RUN_METRICSQL_PARITY=strict`
to fail on any mismatch:

```sh
RUN_METRICSQL_PARITY=strict cargo test -p nmbrs-metricsql --test parity -- --nocapture
```

As of 0.3.0 all 551 cases pass. These fixtures cover parsing and
prettifying; the evaluator is tested by the crate's own unit and
integration tests.

To re-harvest the fixtures from a checkout of the upstream repository
(defaults to `links/metricsql` at the workspace root):

```sh
cargo run -p nmbrs-metricsql --example extract_fixtures -- path/to/metricsql
```

## Cargo features

All features are off by default. With no features, the crate provides
parsing, prettifying, batch evaluation and streaming plans, with no polydat
dependency.

| Feature | Enables |
|---|---|
| `runtime` | The `runtime` module: `ContinuousQueryRuntime`, which owns a set of registered streaming queries, pulls new samples from a `SampleFeed` (for example `PullFeed` over a `MetricAccess`) on each `tick`, and publishes per-query snapshots readable through a `QueryHandle`. Queries are registered with `register` or `register_with` (warmup and `WindowPolicy`). Adds `crossbeam-channel` and `arc-swap`. |
| `polydat-nodes` | The `polydat_nodes` module: the `metricsql`, `metricsql_scalar`, `metricsql_vector` and `metricsql_window` [polydat](https://crates.io/crates/polydat) nodes, which parse a query once, evaluate it against the live metrics service located through `nmbrs_metrics::queryapi::live_access`, and project the result into a polydat `Value`. Adds `polydat` and `inventory`. |

## Links

- Repository: <https://github.com/nosqlbench/nmbrs>
- Crate source: <https://github.com/nosqlbench/nmbrs/tree/main/nmbrs-metricsql>
- API docs: <https://docs.rs/nmbrs-metricsql>
- Design notes: [SRD 08 (MetricsQL)](https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/08_metricsql.md),
  [SRD 47 (streaming)](https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/47_metricsql_streaming.md),
  [SRD 48 (continuous query runtime)](https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/48_metricsql_continuous_query.md)
- Upstream Go implementation: <https://github.com/VictoriaMetrics/metricsql>

## License

Apache-2.0

This crate is a Rust port of
[VictoriaMetrics/metricsql](https://github.com/VictoriaMetrics/metricsql)
and parts of VictoriaMetrics' `vmselect/promql`; its test fixtures are
derived from upstream test files.
