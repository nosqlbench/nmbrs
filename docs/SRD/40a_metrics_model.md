# 40a: Consolidated Metrics Data Model

> **Status: normative.** Mechanical reference — this is the
> single authoritative description of the data model
> (entities, relationships, types, formats, naming rules,
> identity, lookup conventions) shared across:
>
> - **`nmbrs-metrics::snapshot`** — in-memory recording API
>   (the cadence pipeline produces these).
> - **OpenMetrics 1.0** spec (`links/specs/open_metrics_spec.md`,
>   `links/specs/OpenMetrics.md`).
> - **MetricsQL** (`links/specs/MetricsQL.md`) — the
>   query-language overlap (selectors, label semantics).
> - **`nmbrs-metrics::reporters::sqlite`** — durable schema.
> - **`nmbrs-metricsql::catalog`** — read-side catalog trait.

This SRD consolidates the data-model contract so a single
read aligns the in-memory shape, the wire/durable shape,
and the query-language shape. Where the three diverge in
naming or constraints, this document records the rule and
points at the authoritative source.

Cross-refs:

- [SRD 40 — Metrics](40_metrics.md): umbrella for the
  cadence reporting pipeline.
- [SRD 42 — Windowed Metrics](42_windowed_metrics.md):
  cadence-window combination semantics.
- [SRD 47 — MetricsQL Streaming](47_metricsql_streaming.md):
  streaming-evaluator boundary.
- [SRD 49 — MetricsQL Supported Scope](49_metricsql_supported_scope.md):
  what query vocabulary is supported.

---

## 1. Entity-relationship model

The entire model is six entities. Each cell of the table
below is the canonical name, the OpenMetrics spec section,
the Rust type in `nmbrs-metrics::snapshot`, and the SQLite
storage table.

| Entity        | OpenMetrics §  | Rust type                | SQLite                             |
|---------------|----------------|--------------------------|------------------------------------|
| MetricSet     | §4.1           | `MetricSet`              | (implicit — one DB = one MetricSet) |
| MetricFamily  | §4.4           | `MetricFamily`           | `metric_family` (1 row per family) |
| Metric (Series) | §4.5         | `Metric`                 | `metric_instance` (1 row per series) |
| LabelSet      | §4.3           | `Labels`                 | `label_set` + `label_set_entry` (normalised) |
| MetricPoint   | §4.6           | `MetricPoint`            | `sample_value` (1 row per observation) |
| Exemplar      | §4.6.1         | `Exemplar`               | `exemplar` (0..N per `sample_value`) |

```
MetricSet
  └─ MetricFamily          (name, type, unit, help)        unique by `name`
       └─ Metric (Series)  (LabelSet)                      unique by (family.name, LabelSet)
            └─ MetricPoint (value, optional timestamp)
                 └─ Exemplar (LabelSet, value, optional ts)  0..N
```

**Identity rule** (spec §4.5): a series is uniquely
identified by `(MetricFamily.name, LabelSet)`. Two
MetricPoints with the same identity are observations of
the same series at different times.

---

## 2. Types

### 2.1 Metric types (OpenMetrics §4.4)

Every MetricFamily declares one of eight types. Storage
column conventions are pinned in
[SRD 49 §6.1](49_metricsql_supported_scope.md#61-openmetrics-type-coverage)
and replicated below for self-containment.

| Type            | OpenMetrics § | Rust variant                    | SQLite `metric_family.type` | Default sample column |
|-----------------|---------------|---------------------------------|----------------------------|------------------------|
| Counter         | §5.1          | `MetricType::Counter`           | `counter`                  | `count`                |
| Gauge           | §5.2          | `MetricType::Gauge`             | `gauge`                    | `mean`                 |
| Histogram       | §5.3          | `MetricType::Histogram`         | `histogram`                | `count` (cumulative ≤ `le`) |
| GaugeHistogram  | §5.4          | `MetricType::GaugeHistogram`    | `gaugehistogram`           | `count` (non-monotonic) |
| Summary         | §5.5          | `MetricType::Summary`           | `summary`                  | `count`                |
| Info            | §5.6          | `MetricType::Info`              | `info`                     | `count = 1`            |
| StateSet        | §5.7          | `MetricType::StateSet`          | `stateset`                 | `mean ∈ {0, 1}`        |
| Unknown         | §5.8          | `MetricType::Unknown`           | `unknown`                  | `mean` (defensive)     |

### 2.2 Value types (OpenMetrics §4.2)

OpenMetrics requires float64 + int64 with NaN, +Inf, -Inf
support. The Rust model:

| Concept        | Rust type        | Notes                                   |
|----------------|------------------|-----------------------------------------|
| Counter total  | `u64`            | Monotonic, never negative. Resets handled by `created` field. |
| Counter created | `Option<Instant>` | When the series started; resets bump it. |
| Gauge value    | `f64`            | Full IEEE 754 — NaN / ±Inf permitted.  |
| Histogram bucket count | `i64`    | Cumulative count of observations ≤ `le`. |
| Histogram sum / mean / stddev / pXX | `f64` | NaN allowed for empty windows. |
| Sample timestamp | `Instant` (in-memory) / `i64` ms (storage) | Monotonic in-memory, Unix epoch ms on disk. |
| Exemplar value | `f64`            | Single observation. |
| Exemplar timestamp | `Option<Instant>` | Spec §4.6.1: optional. |

### 2.3 Booleans (OpenMetrics §4.2.1)

Booleans MUST follow `1 == true`, `0 == false`. nmbrs follows
this convention everywhere — StateSet stores 0/1 in the
`mean` column; Info stores `1` in the `count` column.

### 2.4 Timestamps (OpenMetrics §4.2.2)

| Layer              | Representation                  |
|--------------------|----------------------------------|
| In-memory          | `std::time::Instant`             |
| SQLite             | `INTEGER NOT NULL` Unix epoch ms |
| MetricsQL queries  | Unix epoch ms                    |
| Wire (text format) | Unix epoch seconds (decimal allowed) |

The text-format conversion (seconds with decimal) only
matters at exposition time and isn't part of the
canonical model.

### 2.5 Strings (OpenMetrics §4.2.3)

Strings MUST be valid UTF-8 and MAY be zero-length. NULL
(0x00) MUST be supported. Rust's `String` satisfies the
former; the latter is honoured by the writer (no trimming)
and the reader (text columns survive 0x00 round-trips per
sqlite's `BLOB`-aware text storage).

---

## 3. Labels (OpenMetrics §4.3)

### 3.1 Label naming rules (ABNF §1)

```text
label-name              = label-name-initial-char *label-name-char
label-name-initial-char = ALPHA / "_"
label-name-char         = label-name-initial-char / DIGIT
```

Concretely: `[A-Za-z_][A-Za-z0-9_]*`. **No colons** (unlike
metric names). Label names beginning with `__` are RESERVED
and MUST NOT be used by application code unless this spec
or OpenMetrics specifies them.

### 3.2 Reserved label names (used by the model)

| Name        | Meaning                                            | Source                              |
|-------------|-----------------------------------------------------|------------------------------------|
| `__name__`  | Synthetic label = MetricFamily name. Set by the catalog reader so consumers can express selectors. | MetricsQL convention |
| `le`        | Bucket boundary on Histogram / GaugeHistogram. Numeric or `+Inf`. | OpenMetrics §5.3 |
| `quantile`  | φ-value on Summary buckets.                         | OpenMetrics §5.5 |

### 3.3 Label values

| Constraint                          | Where enforced              |
|-------------------------------------|------------------------------|
| Valid UTF-8                         | At construction (Rust `String`) |
| Empty label values                  | SHOULD be treated as if the label was not present (spec §4.3). nmbrs preserves them on the wire but normalisation passes drop them. |
| Quote / backslash escaping in text format | At exposition (`reporters::openmetrics`) |

### 3.4 LabelSet identity

Per spec §4.3.2, label names within a LabelSet MUST be
unique. nmbrs's `Labels` type enforces this on construction
(`with(key, value)` overwrites the prior pair for the same
key rather than appending).

LabelSet equality is **set-equal**: two sets with the same
unordered pairs compare equal. nmbrs's `Labels::identity_hash`
sorts pairs before hashing so storage dedup works
regardless of declaration order.

---

## 4. Naming (OpenMetrics §4.4 ABNF)

### 4.1 MetricFamily names

```text
metricname              = metricname-initial-char 0*metricname-char
metricname-initial-char = ALPHA / "_" / ":"
metricname-char         = metricname-initial-char / DIGIT
```

Concretely: `[A-Za-z_:][A-Za-z0-9_:]*`. Note the colon —
metric names accept it, label names don't.

Names beginning with `__` (double underscore) are RESERVED.

### 4.2 Suffix conventions per type (spec §4.4)

OpenMetrics text-format exposition appends type-specific
suffixes:

| Type            | Sample-name suffixes                 |
|-----------------|--------------------------------------|
| Counter         | `_total`, `_created`                 |
| Summary         | `_count`, `_sum`, `_created`, _(empty)_ |
| Histogram       | `_count`, `_sum`, `_bucket`, `_created` |
| GaugeHistogram  | `_gcount`, `_gsum`, `_bucket`        |
| Info            | `_info`                              |
| Gauge           | _(empty)_                            |
| StateSet        | _(empty)_                            |
| Unknown         | _(empty)_                            |

A MetricFamily name MUST NOT clash with any other family's
exposition-suffixed name. nmbrs writers and readers honour
this via the `STAT_SUFFIXES` table in
`nmbrs-metrics::queryapi::sqlite` — bare-name lookup tries
the family name first, then strips known suffixes and
retries.

### 4.3 Unit (OpenMetrics §4.4)

If non-empty, the unit MUST be a suffix of the
MetricFamily name separated by an underscore. Example:
`process_cpu_seconds_total` has unit `seconds`. Stored on
`MetricFamily.unit` (Rust) and `metric_family.unit` (SQLite).

This rule applies uniformly to measured and synthetic metrics:
SRD-40b op-template `metrics: <name>: { unit: <u> }`
declarations populate **both** the family-name suffix
(`<name>_<u>`) and the `metric_family.unit` column. The two
storage sites are derived from the same declaration; there is
no path that sets one without the other.

### 4.4 Help (OpenMetrics §4.4)

Free-form string; SHOULD be non-empty for human consumption.
Stored on `MetricFamily.help` and `metric_family.help`.

---

## 5. Selector / lookup conventions

### 5.1 The series-identity tuple

The canonical key for "what series is this?" everywhere in
nmbrs:

```text
(family_name, LabelSet)        — where LabelSet is the set of (key, value) pairs
                                 minus any reserved names like __name__.
```

`__name__` is **synthesised** at read time from
`family_name` so MetricsQL selectors can target it
directly (`{__name__="foo"}` ≡ `foo{}`).

### 5.2 MetricsQL selectors (overlap subset)

```text
selector       = "{" matcher *("," matcher) "}" / metricname [ "{" *matcher "}" ]
matcher        = label-name matcher-op LITERAL
matcher-op     = "=" / "!=" / "=~" / "!~"
```

A selector compiles to `Vec<Matcher>` (per
`nmbrs-metricsql::eval::Matcher`), and the catalog backend
resolves it to series + samples via the
[`MetricCatalog`](49_metricsql_supported_scope.md) trait.

The resolution algorithm (see
`nmbrs-metrics::queryapi::sqlite::SqliteDataSource`):

1. Find the `__name__` matcher; resolve to `family_id`
   (Eq required by current sqlite impl; regex on
   `__name__` is a future extension).
2. Apply other matchers as INTERSECT against
   `label_set_entry` rows — every constraint must match.
3. Join onto `sample_value` for the time window.

### 5.3 Catalog enumeration shape

[`MetricCatalog`] surfaces four flavours of lookup,
matching the OpenMetrics + Prometheus query API:

| nmbrs method                             | Equivalent OpenMetrics endpoint      |
|-----------------------------------------|---------------------------------------|
| `metric_families()`                     | `/api/v1/metadata`                    |
| `label_keys(family_filter)`             | `/api/v1/labels` (optionally with `match[]`) |
| `label_values(key, family_filter)`      | `/api/v1/label/<key>/values`          |
| `series(matchers)`                      | `/api/v1/series`                      |
| `exemplars(matchers, time_range)`       | `/api/v1/query_exemplars`             |

These together cover the full backend-introspection surface
the catalog needs to drive autocompletion + dashboards.

### 5.4 Lookup-friendly storage shapes

| Concern                | nmbrs convention                         |
|------------------------|------------------------------------------|
| Series-identity hash   | sorted-pair hash on the LabelSet (`Labels::identity_hash`) |
| Family lookup by name  | indexed in SQLite (`UNIQUE(name, type)` on `metric_family`) |
| Label key/value lookup | normalised tables `label_key`, `label_value`, `label_set_entry` with composite index on `(key_id, value_id, set_id)` |
| Time-range scan        | `sample_value(instance_id, timestamp_ms)` index |
| Exemplar lookup        | `exemplar(instance_id, sample_timestamp_ms)` index |

These indexes are the contract for the
`nmbrs-metrics::queryapi::sqlite::SqliteDataSource` query
plans. Removing one degrades to full scans.

---

## 6. Cross-layer invariants

### 6.1 Identity preservation across layers

In-memory → wire → durable → in-memory:

- `MetricFamily.name` is byte-identical at every layer.
- `MetricFamily.r#type()` round-trips through
  `MetricType::as_str()` ↔ `MetricType::parse()`.
- `LabelSet` round-trips through pair-list encoding
  (in-memory `Labels` ↔ on-disk `label_key` × `label_value`
  × `label_set_entry`). Order of pairs is normalised on
  storage; equality is set-equal, not order-equal.
- `MetricPoint.timestamp` round-trips
  `Instant` ↔ Unix epoch ms (modulo a one-time
  process-start anchor).

### 6.2 Combine semantics

Two MetricPoints with the same identity MAY be combined
(SRD-42 §"Combine semantics"):

| Type        | Combine rule                              |
|-------------|-------------------------------------------|
| Counter     | totals add; `created` keeps earliest; exemplar most-recent-wins |
| Gauge       | last-write-wins (most-recent timestamp; OpenMetrics/Prometheus contract — amended 2026-08-08) |
| Histogram (HDR-backed) | reservoirs add via `HdrHistogram::add`; sum/count re-derive; bucket exemplars most-recent-wins per index |

Cross-type combines are forbidden — `combine_into` errors
if the value variants differ.

### 6.3 Validation surface

The Rust API does not enforce all OpenMetrics character
restrictions at construction time (Rust strings are
permissive). Validation responsibilities split:

| Constraint                | Enforced at                  |
|---------------------------|-------------------------------|
| Metric name ABNF          | Exposition (`reporters::openmetrics`) |
| Label name ABNF           | Exposition                    |
| Reserved name (`__*`) check | Exposition (warn, then drop) |
| LabelSet uniqueness       | At construction (`Labels::with`) |
| Unit-is-name-suffix       | At construction (`MetricFamily::with_unit` checks) |
| Family-name suffix clash  | Reader (`STAT_SUFFIXES` resolution) |
| Histogram bucket monotonicity | Producer's responsibility (not enforced post-hoc) |

This split prefers permissive recording + strict exposition:
a workload running under load shouldn't crash because some
ad-hoc `_failed_total_blocked` label name slipped through.

---

## 7. Round-trip pinning (where to find the tests)

| Layer                    | Test location                                                  |
|--------------------------|----------------------------------------------------------------|
| In-memory ↔ in-memory (combine) | `crates/nmbrs-metrics/src/snapshot.rs::tests` |
| In-memory → SQLite       | `crates/nmbrs-metrics/src/reporters/sqlite.rs::tests::sqlite_*` |
| SQLite → catalog         | `crates/nmbrs-metricsql/src/adapters/sqlite.rs::tests::catalog_*` |
| Native types (8 OpenMetrics) | both above, plus `catalog_round_trip_*` per type |
| Exemplars                | `catalog_exemplars_round_trips` (reader), `write_native_sample_*` + future writer test |
| Parser corpus            | `crates/nmbrs-metricsql/tests/parity.rs` against `parser_round_trip.json` |

---

## 8. Gap audit (current state, 2026-05-05)

The columns below answer "is this in nmbrs today?" against
the model defined above.

| Concept                   | In-memory | SQLite | Catalog | Status |
|---------------------------|-----------|--------|---------|--------|
| Counter                   | ✅         | ✅      | ✅       | covered |
| Gauge                     | ✅         | ✅      | ✅       | covered |
| Summary (HDR-backed)      | ✅         | ✅      | ✅       | covered |
| Histogram (bucketed)      | ✅ `MetricValue::BucketedHistogram` | ✅ | ✅ | covered |
| GaugeHistogram            | ✅ `MetricValue::BucketedHistogram` (family-type tag distinguishes) | ✅ | ✅ | covered |
| Info                      | ✅ `MetricValue::Info` | ✅ | ✅ | covered |
| StateSet                  | ✅ `MetricValue::StateSet` | ✅ | ✅ | covered |
| Unknown                   | ✅ (default) | ✅ | ✅ | covered |
| Exemplars (counter)       | ✅ on `CounterValue::exemplar` | ✅ via `exemplar` table | ✅ via `catalog::exemplars()` | covered |
| Exemplars (histogram bucket) | ✅ on `HistogramValue::bucket_exemplars` and `BucketedHistogramValue::bucket_exemplars` | ✅ general-purpose schema | ✅ | covered (HDR-percentile wiring still uses bucket-index slots; explicit-bucket variant uses the same shape directly) |
| Created timestamps        | ✅ on `CounterValue::created` / `HistogramValue::created` / `BucketedHistogramValue::created` | ✅ `sample_value.created_ms` column with auto-migration on existing dbs | ✅ surfaced through the standard `_created` query | covered |
| Multiple MetricPoints per Metric | ✅ `Metric.points: Vec<MetricPoint>` | ✅ multiple `sample_value` rows per `metric_instance` | ✅ `fetch` returns all in window | covered |
| Reserved-label rejection  | ✅ `validation::check_reserved_name` | ✅ | ✅ | covered (recording-side helper; callers opt in via strict mode) |
| Metric-name ABNF          | ✅ `validation::check_metric_name` | n/a | n/a | covered |
| Label-name ABNF           | ✅ `validation::check_label_name` | n/a | n/a | covered |
| Unit-is-name-suffix check | ✅ `validation::check_unit_suffix` | n/a | n/a | covered |
| Bucket-monotonicity check | ✅ `validation::check_bucket_monotonicity` | n/a | n/a | covered (Histogram only; GaugeHistogram exempt per spec) |
| `+Inf` / `-Inf` / `NaN` values | ✅ `f64` accepts them | ✅ `REAL` accepts them | ✅ pass-through | covered |
| LabelSet 128-char limit (exemplars) | ✅ `validation::check_exemplar_length` (per §4.7) | ✅ stored permissively | ✅ pass-through; exposition drops violations | covered (recording-side check; exposition is the strict gate) |

All gaps from the prior audit closed. The validation
helpers live in `nmbrs-metrics::validation` with 19 tests
pinning each rule's coverage; callers decide between
fail-fast (strict mode) / warn / silent-coerce depending
on whether they're recording or exposing data.

---

## 9. Migration / extension lifecycle

When extending the model:

1. **In-memory first.** Add the variant or field on
   `nmbrs-metrics::snapshot`. Combine semantics defined.
2. **Storage second.** Schema migration in
   `nmbrs-metrics::reporters::sqlite::create_schema` —
   `IF NOT EXISTS` so existing dbs pick up the change on
   next open.
3. **Reader third.** Catalog adapter learns the new shape.
   Sample-column routing extended in
   `default_column_for_type`.
4. **Tests at every layer.** Round-trip pinning per §7.
5. **This SRD updated.** Per-row in §1, §2, §8.

Reverse order is a quality bug — the catalog can't surface
data the writer doesn't store.
