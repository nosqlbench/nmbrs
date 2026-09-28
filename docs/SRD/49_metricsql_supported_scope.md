# 49: MetricsQL Supported Scope (canonical reference)

> **Status: normative.** Establishes the supported-scope
> contract for the MetricsQL parser + evaluator + tooling
> (completion, catalog, prettifier).

This SRD names the **single canonical reference** for what
nmbrs supports as MetricsQL. Three sources of truth, all
inside `nmbrs-metricsql`, must agree:

1. **Parser corpus.** The set of inputs the parser accepts
   and round-trips through prettify. Pinned by the
   `parser_round_trip` and `prettifier_round_trip` fixture
   harnesses.
2. **Evaluator dispatch.** The named operations the
   evaluator runs against a [`DataSource`] / [`MetricCatalog`].
3. **Tooling registry.** The static metadata exposed for
   completion, lint, IDE features, and continuous-query
   runtime construction.

When these three diverge, behaviour does — completion offers
something the parser rejects, the evaluator silently no-ops a
named operation, the prettifier emits a token the parser
won't read back. This document fixes the link between them
and pins it in code.

Cross-refs:
- [SRD 47 — MetricsQL Streaming](47_metricsql_streaming.md):
  streaming-evaluator coverage scope.
- [SRD 48 — MetricsQL Continuous Query](48_metricsql_continuous_query.md):
  continuous-query runtime; query construction reads the
  catalog + grammar.
- [SRD 64 — Report CLI](64_report_cli.md): the CLI surface
  that drives `--metric` / `--where` / `--by` completion
  through this stack.

---

## 1. The three sources of truth

### 1.1 Parser corpus

**Authoritative artefact:**
`crates/nmbrs-metricsql/tests/fixtures/parser_round_trip.json` plus
the matching `prettifier_round_trip.json`.

Inputs in these fixtures are guaranteed to:
- parse without error,
- round-trip through the prettifier to the `expected` form,
- preserve every operator, modifier, and function-call shape
  the language admits.

Adding support for a new metricsql shape means adding a
fixture entry. Removing support means deleting one (with
explicit rationale in the commit).

**Aggregate-name surface:** the parser's `is_aggr_func`
function (`crates/nmbrs-metricsql/src/parser.rs`) is the closed list
of names that take aggregate-modifier syntax (`by` / `without`).
Editing this list edits the language surface.

### 1.2 Evaluator dispatch

**Authoritative artefact:** the dispatch enums in
`crates/nmbrs-metricsql/src/eval.rs`:

| Enum                            | Names it handles                              |
|---------------------------------|----------------------------------------------|
| `AggregateOp::from_name`        | `sum avg min max count group stddev stdvar` |
| `ParameterizedAggregateOp::from_name` | `topk bottomk quantile`                |
| `RollupFn::from_name`           | `rate increase delta sum_over_time avg_over_time min_over_time max_over_time count_over_time last_over_time first_over_time stddev_over_time stdvar_over_time` (plus `quantile_over_time` which is dispatched separately because it takes a leading scalar) |

Names not in any of these tables parse fine but the
evaluator returns `EvalError::NotYetImplemented` at run
time — they're **parser-supported, evaluator-deferred**.

### 1.3 Tooling registry

**Authoritative artefact:** `crates/nmbrs-metricsql/src/grammar.rs`
([`AGGREGATE_OPS`], [`ROLLUP_FUNCTIONS`], [`BINARY_OPS`],
modifier constants). Each entry carries:
- the canonical name as the parser expects it,
- an [`EvalSupport`] tag — `ParserAndEval` (entry runs
  through the evaluator) or `ParserOnly` (parses + round-
  trips, evaluator returns NYI),
- structural metadata (takes-param flag, kind classification)
  consumed by completion + lint.

The grammar registry's job is to surface this for tooling
without forcing every consumer to grep the parser source.

---

## 2. Drift detection (in code)

Three test-level pins enforce the link between the three
sources:

1. **`grammar::tests::aggregate_registry_matches_parser_acceptance`**
   parses an upper-case probe expression for every
   [`AGGREGATE_OPS`] entry and verifies the parser's
   case-canonicalisation step (which only fires for
   aggregate-named identifiers) produced a lower-case round
   trip. Behaviourally pins membership against
   `is_aggr_func` without exposing a private helper.
2. **`grammar::tests::rollup_eval_marks_track_evaluator_dispatch`**
   asserts every [`ROLLUP_FUNCTIONS`] entry is
   `EvalSupport::ParserAndEval`. The invariant: rollups don't
   land in the registry until the evaluator wires them; when
   support drops, the entry drops.
3. **`grammar::tests::aggregate_registry_includes_evaluator_subset`**
   asserts the eval-supported aggregate names
   (`sum avg min max count group stddev stdvar topk bottomk
   quantile`) are all present in the registry with
   `EvalSupport::ParserAndEval`. Drift in either direction
   (an entry's support flag flips, or the eval subset
   widens without a registry update) fails here.

The fixture harnesses
(`crates/nmbrs-metricsql/tests/parity.rs`) close the loop on the
parser side: any input the harness exercises is by
definition in-scope for parsing + round-trip.

---

## 3. Lifecycle of an addition

Adding support for a new metricsql operation:

1. **Parser support first.** Add the relevant grammar arm
   (or extend `is_aggr_func`). Add at least one fixture
   entry to `parser_round_trip.json`. Verify the round-trip
   harness lights up the new path.
2. **Evaluator support next.** Add the dispatch arm in
   `eval.rs`. Add an evaluator-level test against a small
   in-memory `DataSource`.
3. **Registry update last.** Add the [`grammar`] entry with
   `EvalSupport::ParserAndEval`. Add the function/aggregate
   to its category table.
4. **Tooling fallout** is automatic — the catalog +
   completion plumbing reads the registry, so the new
   token surfaces in `nmbrs report plot --metric <TAB>`,
   in `nmbrs metrics query` autocomplete, etc.

Reverse order (registry first, evaluator absent) is a
quality bug: tooling promises the user something the
runtime won't deliver.

---

## 4. Lifecycle of a removal / deferral

When the evaluator drops support for a name without the
parser dropping it:

1. Mark the registry entry [`EvalSupport::ParserOnly`].
2. Remove the dispatch arm from `eval.rs`.
3. The drift-detection test
   (`rollup_eval_marks_track_evaluator_dispatch`) currently
   asserts every rollup is `ParserAndEval` — relaxing this
   to "marked support matches dispatch reality" is a
   single-line edit when the case arises.
4. Tooling continues to surface the name with
   `evaluable_only=false`, hidden when the consumer asks
   for `evaluable_only=true`.

When the parser also drops support: drop the registry entry
+ the fixture entry that exercised it.

---

## 5. What sits "outside scope"

The crate's mission is a **subset evaluator** (per
`lib.rs` doc) of upstream MetricsQL. Some upstream features
the parser accepts but the evaluator deliberately doesn't
implement:

- **Transform functions** (`abs`, `ceil`, `clamp`,
  `histogram_quantile`, `round`, `scalar`, `sqrt`,
  `vector`, …) — the evaluator returns NYI; these
  intentionally don't have registry entries until an
  evaluator-side implementation lands.
- **Label-rewriting functions** (`label_join`,
  `label_replace`) — same status.
- **`@<modifier>`** subquery anchors — parses, no eval.
- **`with` expressions** — parser handles them; evaluator
  does not.
- **Variadic aggregates** beyond what the parser accepts.

These aren't "deferred forever" — they're deferred until a
forcing function (a workload or report wanting them) makes
the evaluator-side cost worth paying. Each addition follows
§3.

---

## 6. The catalog dimension

[`MetricCatalog`] is the **data** half of "what we support":
which series exist, what labels they carry, what values
those labels take. Independent from grammar coverage —
catalog data depends on the running session's writer side,
not on what tokens the parser handles.

### 6.1 OpenMetrics type coverage

All eight OpenMetrics 1.0 types are natively round-trippable
through the sqlite schema. Per-type storage convention:

| Type            | `metric_family.type` | Sample columns populated         | Distinguishing label |
|-----------------|----------------------|----------------------------------|----------------------|
| Counter         | `counter`            | `count`                          | (per-instance labels) |
| Gauge           | `gauge`              | `mean`                           | (per-instance labels) |
| Summary         | `summary`            | `count, sum, min, max, mean, stddev, p50–p999` | (per-instance labels) |
| Histogram       | `histogram`          | `count` (cumulative ≤ `le`)      | `le` per bucket       |
| GaugeHistogram  | `gaugehistogram`     | `count` (non-monotonic allowed)  | `le` per bucket       |
| Info            | `info`               | `count = 1` (always)             | descriptive labels    |
| StateSet        | `stateset`           | `mean ∈ {0, 1}`                  | state-name label      |
| Unknown         | `unknown`            | `mean` (defensive fallback)      | (per-instance labels) |

Implied / synthetic labels:
- `le` on `histogram` / `gaugehistogram` — bucket boundary
  (numeric value or `+Inf`).
- `quantile` on `summary` — φ value when summaries are
  exposed in OpenMetrics text format. Internally the
  current writer expands percentiles into per-stat columns
  (`p50`, `p99`, …) rather than per-instance series; both
  shapes are accepted by the catalog reader.
- `__name__` synthesised by [`MetricCatalog::series`] from
  `metric_family.name` so callers can reconstruct a
  selector verbatim.

Derived series (siblings of a histogram/summary family):
- `<name>_bucket` — bucket-counts series; matches the
  catalog by exact name with `le` label distinguishing
  buckets.
- `<name>_sum` / `<name>_count` — cumulative observation
  totals. Stored either as separate families with `_sum`
  / `_count` suffixes, or as instances within the parent
  family carrying special label tags. Both shapes are
  exercised by round-trip tests.

### 6.2 Writer-side native API

`SqliteReporter::write_native_sample` accepts any of the
eight type tags directly via a `NativeSample` row payload.
External producers (or future internal code paths beyond
`Counter` / `Gauge` / HDR-summary) call this to populate
`Histogram` / `GaugeHistogram` / `Info` / `StateSet` /
`Unknown` types without going through the in-memory
`MetricValue` enum.

The high-level `MetricSet::insert_*` API still covers the
three types nmbrs internally produces; the low-level
`write_native_sample` covers the rest.

### 6.3 Round-trip pinning

Each type has a round-trip test pinning the writer's
storage convention against the catalog reader's
interpretation:

- Writer side
  (`crates/nmbrs-metrics/src/reporters/sqlite.rs::tests`):
  `write_native_sample_round_trips_*` for histogram + le
  buckets, info, stateset, gaugehistogram, unknown, plus a
  family/instance-dedup test.
- Reader side
  (`crates/nmbrs-metricsql/src/adapters/sqlite.rs::tests`):
  `catalog_round_trip_*` for histogram, gaugehistogram,
  info, stateset, summary, plus
  `catalog_default_column_for_type_covers_all_eight_types`
  pinning the per-type column-routing convention.

Together these guarantee: any of the eight OpenMetrics
types written via the schema's public API can be read back
through the catalog with the correct type tag, label
vocabulary, and sample-column resolution.

---

## 7. Coverage summary (snapshot)

As of this SRD's writing:

| Surface           | Parser       | Evaluator    | Registry     |
|-------------------|--------------|--------------|--------------|
| Aggregates        | 37 names     | 11 evaluable | 37 entries (11 ParserAndEval, 26 ParserOnly) |
| Rollups           | open         | 13 evaluable | 13 entries (all ParserAndEval) |
| Binary ops        | open         | 15 evaluable | 16 entries (15 ParserAndEval, 1 ParserOnly: `atan2`) |
| Modifiers         | full         | full         | full         |
| Matcher ops       | full (= != =~ !~) | full   | full         |

"open" = the parser accepts arbitrary identifier-followed-by-
parens as a function call; the registry enumerates only what
the evaluator runs. As the evaluator widens, the registry
follows; the drift test pins the link.

---

## 8. Open follow-ups

- **Catalog drift detection.** When the writer-side schema
  adds histogram types, the catalog impl needs the
  corresponding decoder. A schema-version field on
  `metric_family` would let the catalog detect "writer
  version newer than reader knows about" and surface a
  diagnostic — currently absent.
- **Transform / label-fn evaluator support.** Outside §3
  unless a forcing function arrives. Park.
- **MetricCatalog over remote (VictoriaMetrics) backend.**
  The trait is already shaped for it
  (`/api/v1/labels` / `/api/v1/series`); implementation
  waits on a remote read use case.
