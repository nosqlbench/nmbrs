// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Streaming / incremental aggregation for MetricsQL.
//!
//! This module is the implementation of [SRD 47][srd47].
//! Read it before changing the [`Reducer`] trait, the
//! [`StreamingPlan`] compiler, or the property test below —
//! the algebraic invariants documented in the SRD are
//! load-bearing.
//!
//! [srd47]: ../../../docs/SRD/47_metricsql_streaming.md
//!
//! # The three algebraic classes
//!
//! Every metricsql aggregation falls into one of three
//! classes, and which class determines what's possible:
//!
//! | Class | Property | Examples |
//! |-------|----------|----------|
//! | **Distributive** | `f(A ∪ B) = combine(f(A), f(B))`, no extra state. | `sum`, `count`, `min`, `max`, `group`, `*_over_time` (sum/count/min/max/first/last) |
//! | **Algebraic** | Bounded-size accumulator richer than the result. | `avg`, `stddev`, `rate`, `increase`, `delta`, `avg_over_time` |
//! | **Holistic** | Bounded-size *exact* accumulator does not exist. | `quantile`, `topk`, `bottomk`, `count_values`, `mad`, `median` |
//!
//! This module ships the **distributive** subset. Algebraic
//! and holistic reducers ride the same trait and land in
//! their own focused pushes.
//!
//! # The load-bearing guarantee
//!
//! For every supported query shape × every supported reducer,
//! streaming evaluation produces the same result as batch
//! evaluation, regardless of how the input samples are
//! partitioned across `ingest` calls. This is verified by
//! [`tests::streaming_equals_batch_for_supported_shapes`] —
//! the load-bearing artifact. If it ever fails, the algebra
//! is broken; relaxing the test is not the recovery path.
//!
//! # Carve-out: `rate` / `increase` semantics
//!
//! Two functions are deliberately excluded from the
//! batch≡streaming guarantee:
//!
//! - **`rate(...[w])`** and **`increase(...[w])`** —
//!   Batch implements full PromQL semantics: counter-reset
//!   adjustment (walk samples, add back the pre-reset value
//!   on any `xs[i] < xs[i-1]`) and window-edge extrapolation
//!   (scale the rate when the first/last samples don't sit
//!   at the window edges). Both behaviours assume a *fixed*
//!   window with known edges and require walking the full
//!   sample sequence in temporal order.
//!
//!   Streaming sees a *sliding* arrival window: samples land
//!   incrementally, the "window" is whatever the runtime has
//!   ingested so far, and partition order across merge isn't
//!   strictly temporal. Both adjustments would oscillate as
//!   data flows in. The streaming reducer therefore uses the
//!   simpler `(last - first) / window_secs` algebra — which
//!   is what live consumers (TUI panels, dashboards) want
//!   anyway: trend, not exact window-fitted final number.
//!
//!   For batch reports use the eval path; for live runtime
//!   monitoring use streaming. Don't compare them digit-for-
//!   digit.

use crate::ast::{
    AggrModifier, AggrModifierOp, BinaryOp, Expr, FuncExpr, MetricExpr, NumberExpr, RollupExpr,
};
use crate::eval::{Matcher, MatcherOp, Sample, Series};
use std::collections::BTreeMap;

/// Commutative-monoid-with-finalize algebra for streaming
/// aggregation. Implementations MUST satisfy:
///
/// - **identity**: `merge(empty, x) ≡ x`, where
///   `empty` is `Self::Acc::default()`
/// - **commutative**: `merge(a, b) ≡ merge(b, a)`
/// - **associative**: `merge(merge(a, b), c) ≡ merge(a, merge(b, c))`
///
/// `ingest(acc, s)` is equivalent to `merge(acc,
/// single_sample(s))`, exposed separately so the hot path
/// can skip constructing a transient single-sample
/// accumulator.
///
/// Every implementor goes through the streaming-equivalence
/// property test before being considered done — see
/// `tests::streaming_equals_batch_for_supported_shapes`.
pub trait Reducer: Send + Sync {
    /// Accumulator state. Must be cheap to clone (the merge
    /// path takes one by value), default-constructible to
    /// the algebra's identity element, and `Send + Sync` so
    /// the plan can hold heterogeneous reducers and ingest
    /// from any thread.
    type Acc: Clone + Default + Send + Sync;

    /// Fold one new sample into the accumulator. NaN samples
    /// are caller-filtered or reducer-handled depending on
    /// the operator's semantics — most reducers treat NaN as
    /// "no observation" and skip it.
    fn ingest(&self, acc: &mut Self::Acc, sample: &Sample);

    /// Combine two accumulators in place. Must be
    /// commutative and associative; the plan relies on these
    /// laws to make ingest order irrelevant.
    fn merge(&self, into: &mut Self::Acc, other: Self::Acc);

    /// Project the accumulator to the result value. Empty
    /// accumulators (no samples ingested) return NaN per
    /// upstream MetricsQL semantics.
    fn snapshot(&self, acc: &Self::Acc) -> f64;
}

// =============================================================
// Distributive reducers
// =============================================================
//
// Each carries a small, default-constructible accumulator
// and satisfies the three monoid laws by construction.
// Verified per-impl in the `monoid_laws_*` unit tests below.

/// `sum` — adds non-NaN sample values. Uses Kahan summation
/// to keep numerical error bounded and order-independent
/// enough that the equivalence property test passes at
/// `1e-9` tolerance.
pub struct SumReducer;

#[derive(Clone, Default, Debug)]
pub struct KahanAcc {
    /// Running total.
    pub total: f64,
    /// Compensation term — accumulated low-order bits lost
    /// in successive additions. See Kahan 1965.
    pub compensation: f64,
    /// Distinguishes "no samples yet" (snapshot → NaN) from
    /// "samples summed to 0" (snapshot → 0.0).
    pub has_data: bool,
}

impl Reducer for SumReducer {
    type Acc = KahanAcc;

    fn ingest(&self, acc: &mut KahanAcc, sample: &Sample) {
        if sample.value.is_nan() {
            return;
        }
        kahan_add(acc, sample.value);
        acc.has_data = true;
    }

    fn merge(&self, into: &mut KahanAcc, other: KahanAcc) {
        if !other.has_data {
            return;
        }
        // Add `other.total` with the existing compensation,
        // then fold in `other.compensation` separately. This
        // keeps the merge order-independent within tolerance
        // — the property test at 1e-9 catches regressions.
        kahan_add(into, other.total);
        kahan_add(into, other.compensation);
        into.has_data = true;
    }

    fn snapshot(&self, acc: &KahanAcc) -> f64 {
        if !acc.has_data {
            return f64::NAN;
        }
        acc.total
    }
}

fn kahan_add(acc: &mut KahanAcc, x: f64) {
    let y = x - acc.compensation;
    let t = acc.total + y;
    acc.compensation = (t - acc.total) - y;
    acc.total = t;
}

/// `count` — counts non-NaN samples ingested.
pub struct CountReducer;

#[derive(Clone, Default, Debug)]
pub struct CountAcc {
    pub count: u64,
}

impl Reducer for CountReducer {
    type Acc = CountAcc;

    fn ingest(&self, acc: &mut CountAcc, sample: &Sample) {
        if !sample.value.is_nan() {
            acc.count += 1;
        }
    }

    fn merge(&self, into: &mut CountAcc, other: CountAcc) {
        into.count = into.count.saturating_add(other.count);
    }

    fn snapshot(&self, acc: &CountAcc) -> f64 {
        // Empty count is 0.0, NOT NaN — `count(empty)`
        // returns zero per upstream, which differs from the
        // sum / min / max convention.
        acc.count as f64
    }
}

/// `min` — minimum of non-NaN sample values.
pub struct MinReducer;

#[derive(Clone, Default, Debug)]
pub struct MinAcc {
    /// Defaults to `+∞` lazily — we don't initialise it at
    /// `Default::default()` because `f64::INFINITY` isn't
    /// the type's default; track presence explicitly via
    /// `has_data` and treat the field as undefined until
    /// then.
    pub value: f64,
    pub has_data: bool,
}

impl Reducer for MinReducer {
    type Acc = MinAcc;

    fn ingest(&self, acc: &mut MinAcc, sample: &Sample) {
        if sample.value.is_nan() {
            return;
        }
        if !acc.has_data || sample.value < acc.value {
            acc.value = sample.value;
        }
        acc.has_data = true;
    }

    fn merge(&self, into: &mut MinAcc, other: MinAcc) {
        if !other.has_data {
            return;
        }
        if !into.has_data || other.value < into.value {
            into.value = other.value;
        }
        into.has_data = true;
    }

    fn snapshot(&self, acc: &MinAcc) -> f64 {
        if !acc.has_data {
            return f64::NAN;
        }
        acc.value
    }
}

/// `max` — maximum of non-NaN sample values.
pub struct MaxReducer;

#[derive(Clone, Default, Debug)]
pub struct MaxAcc {
    pub value: f64,
    pub has_data: bool,
}

impl Reducer for MaxReducer {
    type Acc = MaxAcc;

    fn ingest(&self, acc: &mut MaxAcc, sample: &Sample) {
        if sample.value.is_nan() {
            return;
        }
        if !acc.has_data || sample.value > acc.value {
            acc.value = sample.value;
        }
        acc.has_data = true;
    }

    fn merge(&self, into: &mut MaxAcc, other: MaxAcc) {
        if !other.has_data {
            return;
        }
        if !into.has_data || other.value > into.value {
            into.value = other.value;
        }
        into.has_data = true;
    }

    fn snapshot(&self, acc: &MaxAcc) -> f64 {
        if !acc.has_data {
            return f64::NAN;
        }
        acc.value
    }
}

/// `group` — emits 1.0 for any group with at least one
/// non-NaN sample, NaN otherwise. Used to materialize a
/// label-set with no data dependency.
pub struct GroupReducer;

#[derive(Clone, Default, Debug)]
pub struct GroupAcc {
    pub has_data: bool,
}

impl Reducer for GroupReducer {
    type Acc = GroupAcc;

    fn ingest(&self, acc: &mut GroupAcc, sample: &Sample) {
        if !sample.value.is_nan() {
            acc.has_data = true;
        }
    }

    fn merge(&self, into: &mut GroupAcc, other: GroupAcc) {
        into.has_data = into.has_data || other.has_data;
    }

    fn snapshot(&self, acc: &GroupAcc) -> f64 {
        if acc.has_data { 1.0 } else { f64::NAN }
    }
}

// =============================================================
// Algebraic reducers
// =============================================================
//
// Bounded-size accumulator richer than the result. Each
// satisfies the same monoid laws as the distributive set —
// the property test in `tests` runs them through the same
// random-partition / merge / batch-equivalence check.

/// `avg` and `avg_over_time` — running mean. Carries
/// `(sum, count)`; merge sums both, snapshot divides.
pub struct AvgReducer;

#[derive(Clone, Default, Debug)]
pub struct AvgAcc {
    /// Kahan-summed total — keeps avg numerically close to
    /// batch evaluation across arbitrary partitions.
    pub sum: KahanAcc,
    pub count: u64,
}

impl Reducer for AvgReducer {
    type Acc = AvgAcc;

    fn ingest(&self, acc: &mut AvgAcc, sample: &Sample) {
        if sample.value.is_nan() {
            return;
        }
        kahan_add(&mut acc.sum, sample.value);
        acc.sum.has_data = true;
        acc.count += 1;
    }

    fn merge(&self, into: &mut AvgAcc, other: AvgAcc) {
        // Reuse Sum's Kahan-stable merge, then add counts.
        SumReducer.merge(&mut into.sum, other.sum);
        into.count = into.count.saturating_add(other.count);
    }

    fn snapshot(&self, acc: &AvgAcc) -> f64 {
        if acc.count == 0 {
            return f64::NAN;
        }
        acc.sum.total / acc.count as f64
    }
}

/// Welford accumulator shared by `stddev` and `stdvar`.
/// Ingest follows Welford 1962 (numerically stable running
/// mean + M2). Merge follows Chan-Golub-LeVeque 1979
/// (parallel variance combine).
#[derive(Clone, Default, Debug)]
pub struct WelfordAcc {
    pub count: u64,
    pub mean: f64,
    /// Sum of squared deviations from the running mean.
    /// `variance = m2 / count`; `stddev = sqrt(variance)`.
    pub m2: f64,
}

fn welford_ingest(acc: &mut WelfordAcc, value: f64) {
    if value.is_nan() {
        return;
    }
    acc.count += 1;
    let delta = value - acc.mean;
    acc.mean += delta / acc.count as f64;
    let delta2 = value - acc.mean;
    acc.m2 += delta * delta2;
}

fn welford_merge(into: &mut WelfordAcc, other: WelfordAcc) {
    if other.count == 0 {
        return;
    }
    if into.count == 0 {
        *into = other;
        return;
    }
    // Chan-Golub-LeVeque 1979 parallel variance.
    let delta = other.mean - into.mean;
    let combined_count = into.count + other.count;
    let new_mean =
        (into.count as f64 * into.mean + other.count as f64 * other.mean) / combined_count as f64;
    let new_m2 = into.m2
        + other.m2
        + delta * delta * (into.count as f64 * other.count as f64) / combined_count as f64;
    into.count = combined_count;
    into.mean = new_mean;
    into.m2 = new_m2;
}

/// `stddev` / `stddev_over_time` — population stddev,
/// `sqrt(M2 / N)`. Empty / single-sample groups yield NaN
/// for empty, 0 for size 1 — matches upstream Prometheus.
pub struct StddevReducer;

impl Reducer for StddevReducer {
    type Acc = WelfordAcc;

    fn ingest(&self, acc: &mut WelfordAcc, sample: &Sample) {
        welford_ingest(acc, sample.value);
    }

    fn merge(&self, into: &mut WelfordAcc, other: WelfordAcc) {
        welford_merge(into, other);
    }

    fn snapshot(&self, acc: &WelfordAcc) -> f64 {
        if acc.count == 0 {
            return f64::NAN;
        }
        (acc.m2 / acc.count as f64).sqrt()
    }
}

/// `stdvar` / `stdvar_over_time` — population variance,
/// `M2 / N`. Same accumulator as `stddev`; the snapshot is
/// the squared form.
pub struct StdvarReducer;

impl Reducer for StdvarReducer {
    type Acc = WelfordAcc;

    fn ingest(&self, acc: &mut WelfordAcc, sample: &Sample) {
        welford_ingest(acc, sample.value);
    }

    fn merge(&self, into: &mut WelfordAcc, other: WelfordAcc) {
        welford_merge(into, other);
    }

    fn snapshot(&self, acc: &WelfordAcc) -> f64 {
        if acc.count == 0 {
            return f64::NAN;
        }
        acc.m2 / acc.count as f64
    }
}

/// Time-stamped first AND last accumulator — backs `rate`,
/// `increase`, and `delta`. Merge picks first by smallest
/// `first_ts` and last by largest `last_ts`, so partition
/// order doesn't matter even though the operation is
/// "directional" semantically.
#[derive(Clone, Default, Debug)]
pub struct FirstLastAcc {
    pub first_value: f64,
    pub first_ts: i64,
    pub last_value: f64,
    pub last_ts: i64,
    pub has_data: bool,
}

fn first_last_ingest(acc: &mut FirstLastAcc, sample: &Sample) {
    if sample.value.is_nan() {
        return;
    }
    if !acc.has_data {
        acc.first_value = sample.value;
        acc.first_ts = sample.timestamp_ms;
        acc.last_value = sample.value;
        acc.last_ts = sample.timestamp_ms;
        acc.has_data = true;
        return;
    }
    if sample.timestamp_ms < acc.first_ts {
        acc.first_value = sample.value;
        acc.first_ts = sample.timestamp_ms;
    }
    if sample.timestamp_ms > acc.last_ts {
        acc.last_value = sample.value;
        acc.last_ts = sample.timestamp_ms;
    }
}

fn first_last_merge(into: &mut FirstLastAcc, other: FirstLastAcc) {
    if !other.has_data {
        return;
    }
    if !into.has_data {
        *into = other;
        return;
    }
    if other.first_ts < into.first_ts {
        into.first_value = other.first_value;
        into.first_ts = other.first_ts;
    }
    if other.last_ts > into.last_ts {
        into.last_value = other.last_value;
        into.last_ts = other.last_ts;
    }
}

/// `increase` and `delta` — `last_value - first_value` over
/// the window. Counter resets aren't detected here (no
/// per-counter type info at the streaming layer); for
/// monotonic counters this is the increase, for gauges it's
/// the delta. Same algebra either way.
pub struct IncreaseReducer;

impl Reducer for IncreaseReducer {
    type Acc = FirstLastAcc;

    fn ingest(&self, acc: &mut FirstLastAcc, sample: &Sample) {
        first_last_ingest(acc, sample);
    }

    fn merge(&self, into: &mut FirstLastAcc, other: FirstLastAcc) {
        first_last_merge(into, other);
    }

    fn snapshot(&self, acc: &FirstLastAcc) -> f64 {
        if !acc.has_data {
            return f64::NAN;
        }
        acc.last_value - acc.first_value
    }
}

/// `rate` — per-second rate over the rollup window.
/// `(last - first) / (window_seconds)` — no extrapolation
/// for partial windows, no counter-reset adjustment (both
/// land alongside subqueries when those arrive). The
/// reducer carries `window_ms` so the snapshot can divide;
/// the plan compiler binds the value at construction time.
pub struct RateReducer {
    pub window_ms: i64,
}

impl Reducer for RateReducer {
    type Acc = FirstLastAcc;

    fn ingest(&self, acc: &mut FirstLastAcc, sample: &Sample) {
        first_last_ingest(acc, sample);
    }

    fn merge(&self, into: &mut FirstLastAcc, other: FirstLastAcc) {
        first_last_merge(into, other);
    }

    fn snapshot(&self, acc: &FirstLastAcc) -> f64 {
        if !acc.has_data {
            return f64::NAN;
        }
        if self.window_ms <= 0 {
            return f64::NAN;
        }
        (acc.last_value - acc.first_value) / (self.window_ms as f64 / 1000.0)
    }
}

// =============================================================
// Holistic reducers (HDR-sketch backed)
// =============================================================
//
// SRD-47 §"Holistic-function policy" picks the HDR-sketch
// option as the default — bounded memory, mergeable
// (`Histogram::add` is associative + commutative), bounded
// relative error per HDR's `sigfigs`. The trade-off vs
// exact computation is acknowledged: the property test
// passes because both batch and streaming go through the
// same sketch.
//
// HDR records `u64`. `f64` samples are floored to integers;
// negative values and NaN are dropped. Real consumers in
// nmbrs query latency-style metrics (non-negative integer
// nanoseconds), so the conversion is lossless in practice.

const HDR_SIGFIGS: u8 = 3;
const HDR_LOW: u64 = 1;
const HDR_HIGH: u64 = 1_000_000_000_000; // 1e12 — covers ns up to ~16 minutes

/// HDR histogram accumulator. Wraps the upstream type so
/// `Default` and `Clone` are well-defined for the AccCell
/// dispatch — both are derivable through the wrapper but
/// not directly on the foreign type.
pub struct HdrAcc {
    pub hist: hdrhistogram::Histogram<u64>,
}

impl Default for HdrAcc {
    fn default() -> Self {
        let hist = hdrhistogram::Histogram::<u64>::new_with_bounds(HDR_LOW, HDR_HIGH, HDR_SIGFIGS)
            .expect("HDR construction with valid bounds");
        Self { hist }
    }
}

impl Clone for HdrAcc {
    fn clone(&self) -> Self {
        Self {
            hist: self.hist.clone(),
        }
    }
}

impl std::fmt::Debug for HdrAcc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "HdrAcc(count={}, min={}, max={})",
            self.hist.len(),
            self.hist.min(),
            self.hist.max()
        )
    }
}

/// `quantile_over_time(phi, metric[w])` — phi-quantile of
/// the values in the rollup window, computed via HDR
/// histogram.
pub struct QuantileOverTimeReducer {
    pub quantile: f64,
}

impl Reducer for QuantileOverTimeReducer {
    type Acc = HdrAcc;

    fn ingest(&self, acc: &mut HdrAcc, sample: &Sample) {
        if sample.value.is_nan() {
            return;
        }
        if sample.value < 0.0 {
            return;
        }
        let v = sample.value.floor().min(HDR_HIGH as f64) as u64;
        let v = v.max(HDR_LOW);
        let _ = acc.hist.record(v);
    }

    fn merge(&self, into: &mut HdrAcc, other: HdrAcc) {
        // `Histogram::add` is the load-bearing mergeable
        // operation — associative and commutative. Failure
        // here would mean the histograms have incompatible
        // bounds, which can't happen since we always use
        // the same constants.
        let _ = into.hist.add(other.hist);
    }

    fn snapshot(&self, acc: &HdrAcc) -> f64 {
        if acc.hist.is_empty() {
            return f64::NAN;
        }
        acc.hist.value_at_quantile(self.quantile) as f64
    }
}

// =============================================================
// Timestamp-aware reducers: first / last
// =============================================================
//
// `sum_over_time` / `count_over_time` / `min_over_time` /
// `max_over_time` reuse [`SumReducer`] / [`CountReducer`] /
// [`MinReducer`] / [`MaxReducer`] — the algebra is identical,
// only the binding is different (across-series vs.
// within-series-across-time, which is a plan-compiler
// concern, not a reducer concern).
//
// `first_over_time` / `last_over_time` need explicit
// timestamp-aware accumulators: the per-sample compare key
// is the *timestamp*, not the value.

/// `first_over_time` — value of the earliest non-NaN sample.
pub struct FirstOverTimeReducer;

#[derive(Clone, Default, Debug)]
pub struct FirstAcc {
    pub value: f64,
    pub timestamp_ms: i64,
    pub has_data: bool,
}

impl Reducer for FirstOverTimeReducer {
    type Acc = FirstAcc;

    fn ingest(&self, acc: &mut FirstAcc, sample: &Sample) {
        if sample.value.is_nan() {
            return;
        }
        if !acc.has_data || sample.timestamp_ms < acc.timestamp_ms {
            acc.value = sample.value;
            acc.timestamp_ms = sample.timestamp_ms;
        }
        acc.has_data = true;
    }

    fn merge(&self, into: &mut FirstAcc, other: FirstAcc) {
        if !other.has_data {
            return;
        }
        if !into.has_data || other.timestamp_ms < into.timestamp_ms {
            into.value = other.value;
            into.timestamp_ms = other.timestamp_ms;
        }
        into.has_data = true;
    }

    fn snapshot(&self, acc: &FirstAcc) -> f64 {
        if !acc.has_data {
            return f64::NAN;
        }
        acc.value
    }
}

/// `last_over_time` — value of the latest non-NaN sample.
pub struct LastOverTimeReducer;

#[derive(Clone, Default, Debug)]
pub struct LastAcc {
    pub value: f64,
    pub timestamp_ms: i64,
    pub has_data: bool,
}

impl Reducer for LastOverTimeReducer {
    type Acc = LastAcc;

    fn ingest(&self, acc: &mut LastAcc, sample: &Sample) {
        if sample.value.is_nan() {
            return;
        }
        if !acc.has_data || sample.timestamp_ms > acc.timestamp_ms {
            acc.value = sample.value;
            acc.timestamp_ms = sample.timestamp_ms;
        }
        acc.has_data = true;
    }

    fn merge(&self, into: &mut LastAcc, other: LastAcc) {
        if !other.has_data {
            return;
        }
        if !into.has_data || other.timestamp_ms > into.timestamp_ms {
            into.value = other.value;
            into.timestamp_ms = other.timestamp_ms;
        }
        into.has_data = true;
    }

    fn snapshot(&self, acc: &LastAcc) -> f64 {
        if !acc.has_data {
            return f64::NAN;
        }
        acc.value
    }
}

// =============================================================
// Reducer dispatch — enum-erased heterogeneous reducers
// =============================================================
//
// `StreamingPlan` holds reducers and accumulators
// heterogeneously (one map keyed on group key, values of
// different `Acc` types depending on the reducer). Static
// dispatch over a closed enum keeps the trait surface
// generic while avoiding `Box<dyn Reducer>` and the
// associated-type erasure dance — we know the reducer set
// at compile time.

/// Closed enumeration of the reducer kinds the streaming
/// layer ships. Adding a reducer is two edits: add a variant
/// here + add the matching arm in [`AccCell`] and the four
/// dispatch methods below.
///
/// `Eq` is intentionally not derived — the `QuantileOverTime`
/// variant carries an `f64` (NaN-tolerant), which `Eq` rules
/// out. Tests use `PartialEq` matchers via `matches!()` for
/// parameterized variants.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ReducerKind {
    // ---- distributive ----
    /// `sum(...)` and `sum_over_time(...[w])` — same reducer,
    /// the plan pipeline binds it differently.
    Sum,
    /// `count(...)` and `count_over_time(...[w])`.
    Count,
    /// `min(...)` and `min_over_time(...[w])`.
    Min,
    /// `max(...)` and `max_over_time(...[w])`.
    Max,
    /// `group(...)` — emits 1.0 per group with any data.
    Group,
    /// `first_over_time(...[w])` — timestamp-aware.
    FirstOverTime,
    /// `last_over_time(...[w])` — timestamp-aware.
    LastOverTime,

    // ---- algebraic ----
    /// `avg(...)` and `avg_over_time(...[w])`.
    Avg,
    /// `stddev(...)` and `stddev_over_time(...[w])`.
    Stddev,
    /// `stdvar(...)` and `stdvar_over_time(...[w])`.
    Stdvar,
    /// `increase(...[w])` — `last_value - first_value` over
    /// the window. Per-series rollup only.
    Increase,
    /// `delta(...[w])` — same algebra as `increase` but
    /// semantically for gauges; preserved as a distinct
    /// kind so query round-tripping keeps the user's intent.
    Delta,
    /// `rate(...[w])` — `increase / window_seconds`. The
    /// `window_ms` is bound at compile time by the plan
    /// compiler; classify-time placeholder is `0`.
    Rate { window_ms: i64 },

    // ---- holistic (HDR-sketch backed) ----
    /// `quantile_over_time(phi, ...[w])` — phi-quantile of
    /// the values in the window. Bounded memory, bounded
    /// relative error; mergeable.
    QuantileOverTime { quantile: f64 },
}

/// Erased accumulator cell. Each variant matches one
/// [`ReducerKind`]; the dispatch methods below downcast via
/// `match` (no `Any`, no runtime type checks).
///
/// Note: some accumulator shapes are shared across multiple
/// reducer kinds — `Welford` backs both `Stddev` and
/// `Stdvar`; `FirstLast` backs `Increase`, `Delta`, and
/// `Rate`. The dispatch knows which reducer's snapshot to
/// run for each kind/cell pair.
#[derive(Clone, Debug)]
pub enum AccCell {
    Sum(KahanAcc),
    Count(CountAcc),
    Min(MinAcc),
    Max(MaxAcc),
    Group(GroupAcc),
    First(FirstAcc),
    Last(LastAcc),
    Avg(AvgAcc),
    Welford(WelfordAcc),
    FirstLast(FirstLastAcc),
    Hdr(HdrAcc),
}

impl ReducerKind {
    /// Produce the identity accumulator for this kind. The
    /// returned variant matches the kind 1:1; the dispatch
    /// methods below assume that pairing — mismatches would
    /// be a programmer error.
    pub fn empty(self) -> AccCell {
        match self {
            ReducerKind::Sum => AccCell::Sum(KahanAcc::default()),
            ReducerKind::Count => AccCell::Count(CountAcc::default()),
            ReducerKind::Min => AccCell::Min(MinAcc::default()),
            ReducerKind::Max => AccCell::Max(MaxAcc::default()),
            ReducerKind::Group => AccCell::Group(GroupAcc::default()),
            ReducerKind::FirstOverTime => AccCell::First(FirstAcc::default()),
            ReducerKind::LastOverTime => AccCell::Last(LastAcc::default()),
            ReducerKind::Avg => AccCell::Avg(AvgAcc::default()),
            ReducerKind::Stddev => AccCell::Welford(WelfordAcc::default()),
            ReducerKind::Stdvar => AccCell::Welford(WelfordAcc::default()),
            ReducerKind::Increase => AccCell::FirstLast(FirstLastAcc::default()),
            ReducerKind::Delta => AccCell::FirstLast(FirstLastAcc::default()),
            ReducerKind::Rate { .. } => AccCell::FirstLast(FirstLastAcc::default()),
            ReducerKind::QuantileOverTime { .. } => AccCell::Hdr(HdrAcc::default()),
        }
    }

    pub fn ingest(self, acc: &mut AccCell, sample: &Sample) {
        match (self, acc) {
            (ReducerKind::Sum, AccCell::Sum(a)) => SumReducer.ingest(a, sample),
            (ReducerKind::Count, AccCell::Count(a)) => CountReducer.ingest(a, sample),
            (ReducerKind::Min, AccCell::Min(a)) => MinReducer.ingest(a, sample),
            (ReducerKind::Max, AccCell::Max(a)) => MaxReducer.ingest(a, sample),
            (ReducerKind::Group, AccCell::Group(a)) => GroupReducer.ingest(a, sample),
            (ReducerKind::FirstOverTime, AccCell::First(a)) => {
                FirstOverTimeReducer.ingest(a, sample)
            }
            (ReducerKind::LastOverTime, AccCell::Last(a)) => LastOverTimeReducer.ingest(a, sample),
            (ReducerKind::Avg, AccCell::Avg(a)) => AvgReducer.ingest(a, sample),
            (ReducerKind::Stddev, AccCell::Welford(a)) => StddevReducer.ingest(a, sample),
            (ReducerKind::Stdvar, AccCell::Welford(a)) => StdvarReducer.ingest(a, sample),
            (ReducerKind::Increase, AccCell::FirstLast(a)) => IncreaseReducer.ingest(a, sample),
            (ReducerKind::Delta, AccCell::FirstLast(a)) => IncreaseReducer.ingest(a, sample),
            (ReducerKind::Rate { window_ms }, AccCell::FirstLast(a)) => {
                RateReducer { window_ms }.ingest(a, sample)
            }
            (ReducerKind::QuantileOverTime { quantile }, AccCell::Hdr(a)) => {
                QuantileOverTimeReducer { quantile }.ingest(a, sample)
            }
            _ => unreachable!("ReducerKind/AccCell mismatch — `empty()` produces matched pairs"),
        }
    }

    pub fn merge(self, into: &mut AccCell, other: AccCell) {
        match (self, into, other) {
            (ReducerKind::Sum, AccCell::Sum(a), AccCell::Sum(b)) => SumReducer.merge(a, b),
            (ReducerKind::Count, AccCell::Count(a), AccCell::Count(b)) => CountReducer.merge(a, b),
            (ReducerKind::Min, AccCell::Min(a), AccCell::Min(b)) => MinReducer.merge(a, b),
            (ReducerKind::Max, AccCell::Max(a), AccCell::Max(b)) => MaxReducer.merge(a, b),
            (ReducerKind::Group, AccCell::Group(a), AccCell::Group(b)) => GroupReducer.merge(a, b),
            (ReducerKind::FirstOverTime, AccCell::First(a), AccCell::First(b)) => {
                FirstOverTimeReducer.merge(a, b)
            }
            (ReducerKind::LastOverTime, AccCell::Last(a), AccCell::Last(b)) => {
                LastOverTimeReducer.merge(a, b)
            }
            (ReducerKind::Avg, AccCell::Avg(a), AccCell::Avg(b)) => AvgReducer.merge(a, b),
            (ReducerKind::Stddev, AccCell::Welford(a), AccCell::Welford(b)) => {
                StddevReducer.merge(a, b)
            }
            (ReducerKind::Stdvar, AccCell::Welford(a), AccCell::Welford(b)) => {
                StdvarReducer.merge(a, b)
            }
            (ReducerKind::Increase, AccCell::FirstLast(a), AccCell::FirstLast(b)) => {
                IncreaseReducer.merge(a, b)
            }
            (ReducerKind::Delta, AccCell::FirstLast(a), AccCell::FirstLast(b)) => {
                IncreaseReducer.merge(a, b)
            }
            (ReducerKind::Rate { window_ms }, AccCell::FirstLast(a), AccCell::FirstLast(b)) => {
                RateReducer { window_ms }.merge(a, b)
            }
            (ReducerKind::QuantileOverTime { quantile }, AccCell::Hdr(a), AccCell::Hdr(b)) => {
                QuantileOverTimeReducer { quantile }.merge(a, b)
            }
            _ => unreachable!("ReducerKind/AccCell mismatch in merge"),
        }
    }

    pub fn snapshot(self, acc: &AccCell) -> f64 {
        match (self, acc) {
            (ReducerKind::Sum, AccCell::Sum(a)) => SumReducer.snapshot(a),
            (ReducerKind::Count, AccCell::Count(a)) => CountReducer.snapshot(a),
            (ReducerKind::Min, AccCell::Min(a)) => MinReducer.snapshot(a),
            (ReducerKind::Max, AccCell::Max(a)) => MaxReducer.snapshot(a),
            (ReducerKind::Group, AccCell::Group(a)) => GroupReducer.snapshot(a),
            (ReducerKind::FirstOverTime, AccCell::First(a)) => FirstOverTimeReducer.snapshot(a),
            (ReducerKind::LastOverTime, AccCell::Last(a)) => LastOverTimeReducer.snapshot(a),
            (ReducerKind::Avg, AccCell::Avg(a)) => AvgReducer.snapshot(a),
            (ReducerKind::Stddev, AccCell::Welford(a)) => StddevReducer.snapshot(a),
            (ReducerKind::Stdvar, AccCell::Welford(a)) => StdvarReducer.snapshot(a),
            (ReducerKind::Increase, AccCell::FirstLast(a)) => IncreaseReducer.snapshot(a),
            (ReducerKind::Delta, AccCell::FirstLast(a)) => IncreaseReducer.snapshot(a),
            (ReducerKind::Rate { window_ms }, AccCell::FirstLast(a)) => {
                RateReducer { window_ms }.snapshot(a)
            }
            (ReducerKind::QuantileOverTime { quantile }, AccCell::Hdr(a)) => {
                QuantileOverTimeReducer { quantile }.snapshot(a)
            }
            _ => unreachable!("ReducerKind/AccCell mismatch in snapshot"),
        }
    }

    /// Bind the rollup window to a `Rate` kind. No-op for
    /// every other reducer. Called by the plan compiler when
    /// it has parsed the `[w]` modifier and knows the window
    /// length the rate snapshot needs to divide by.
    pub fn bind_window(self, window_ms: i64) -> Self {
        match self {
            ReducerKind::Rate { .. } => ReducerKind::Rate { window_ms },
            other => other,
        }
    }
}

// =============================================================
// StreamingPlan
// =============================================================

/// Compile-time error from [`compile_streaming`]. Always
/// names the reason — the supported subset grows by adding
/// reducers and shapes, not by relaxing the compiler.
#[derive(Debug, Clone)]
pub enum CompileError {
    /// AST node shape isn't supported by the streaming
    /// compiler in this push (binary ops, algebraic
    /// reducers, quantile, vector matching modifiers, etc.).
    /// The reason text names the shape so callers can
    /// produce useful diagnostics.
    Unsupported(String),
    /// AST is structurally invalid for a streaming plan
    /// (e.g. nested aggregates, missing window).
    InvalidShape(String),
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::Unsupported(s) => write!(f, "streaming compile: unsupported: {s}"),
            CompileError::InvalidShape(s) => write!(f, "streaming compile: invalid shape: {s}"),
        }
    }
}

impl std::error::Error for CompileError {}

/// Grouping mode for an aggregate stage. Mirrors the
/// `by(...)` / `without(...)` modifier on [`FuncExpr`];
/// `All` is the no-modifier case (every series collapses
/// into the same empty-keyed bucket).
#[derive(Clone, Debug)]
pub enum Grouping {
    All,
    By(Vec<String>),
    Without(Vec<String>),
}

/// A compiled streaming query plan.
///
/// Holds reducer state and dispatches per-sample. Construct
/// via [`compile_streaming`]; ingest series data through
/// [`StreamingPlan::ingest`]; read the current result via
/// [`StreamingPlan::snapshot`].
///
/// Scalar postprocess: a query like `rate(cpu[5m]) * 100`
/// is one vector pipeline plus a chain of scalar
/// transforms. The vector side accumulates incrementally;
/// the postprocesses run at snapshot time.
///
/// Vector/vector binary ops live in the recursive [`Node`]
/// tree — `a + b` becomes `Node::Binary { left: Leaf(a),
/// right: Leaf(b), op: Add }`. Each leaf maintains its own
/// matchers; ingest walks the tree, samples are filtered at
/// each leaf independently.
#[derive(Debug)]
pub struct StreamingPlan {
    root: Node,
}

/// Recursive plan-tree node. Either a leaf with its own
/// matchers + reducer pipeline, or an inner binary node
/// combining two children at snapshot time.
#[derive(Debug)]
enum Node {
    Leaf(LeafState),
    Binary {
        op: BinaryOp,
        bool_modifier: bool,
        group_modifier: Option<crate::ast::GroupModifier>,
        join_modifier: Option<crate::ast::JoinModifier>,
        left: Box<Node>,
        right: Box<Node>,
    },
}

/// Leaf node state — the matcher set, per-pipeline
/// accumulator structure, and any scalar postprocess chain
/// that wraps it. Per-leaf chains let `(a * 2) + (b * 3)`
/// keep its scalar transforms attached to the right
/// vector — outer plan no longer needs a global chain.
#[derive(Debug)]
struct LeafState {
    matchers: Vec<Matcher>,
    pipeline: Pipeline,
    scalar_postprocesses: Vec<ScalarPost>,
}

/// A scalar binary op applied at snapshot time. Encodes the
/// op and operand-position so non-commutative ops like `-`
/// and `/` get the source orientation.
#[derive(Clone, Copy, Debug)]
struct ScalarPost {
    op: BinaryOp,
    scalar: f64,
    /// True when the scalar is on the LEFT of the op
    /// (`100 - cpu`); false when on the RIGHT (`cpu - 100`).
    scalar_on_left: bool,
    /// `bool` modifier for comparison ops (`>bool 50`).
    bool_modifier: bool,
}

/// The four supported pipeline shapes — see SRD-47
/// §"Plan compilation" for the AST patterns each maps to.
#[derive(Debug)]
enum Pipeline {
    /// `<selector>` — bare passthrough; ingest stores per-series.
    /// Mostly for tests / completeness; production queries
    /// always wrap a selector in a reducer.
    Bare {
        per_series: BTreeMap<Vec<(String, String)>, Vec<Sample>>,
    },
    /// `<agg>(<selector>) [by|without (...)]`
    ///
    /// Per-timestamp accumulator nested inside per-group:
    /// batch `sum(cpu)` aggregates the *same* timestamp's
    /// samples across series, so the streaming model must
    /// key the same way to be batch-equivalent. Memory is
    /// `O(group_cardinality × distinct_timestamps)` —
    /// bounded by retention horizon for continuous queries.
    Aggregate {
        reducer: ReducerKind,
        grouping: Grouping,
        groups: BTreeMap<Vec<(String, String)>, BTreeMap<i64, AccCell>>,
    },
    /// `<rollup_fn>(<selector>[w])` — windowed reducer per
    /// input series. The `window_ms` is retained for the
    /// future sliding-window framing; for this push the
    /// snapshot reads whatever has accumulated since the
    /// last `reset`.
    Window {
        reducer: ReducerKind,
        // Held for future sliding-window framing; not used
        // in this push.
        #[allow(dead_code)]
        window_ms: i64,
        per_series: BTreeMap<Vec<(String, String)>, AccCell>,
    },
    /// `<agg>(<rollup_fn>(<selector>[w])) [by|without (...)]`
    /// — windowed inner reducer feeds an outer aggregate at
    /// snapshot time.
    WindowedAggregate {
        window_reducer: ReducerKind,
        // Same: retained for sliding-window framing.
        #[allow(dead_code)]
        window_ms: i64,
        per_series: BTreeMap<Vec<(String, String)>, AccCell>,
        outer_reducer: ReducerKind,
        grouping: Grouping,
    },
}

// =============================================================
// StreamingPlan: ingest / snapshot data path
// =============================================================

impl Pipeline {
    /// Per-pipeline ingest. `labels` are pre-filtered by the
    /// caller (the leaf node checks matchers before calling
    /// here).
    fn ingest(&mut self, labels: &[(String, String)], sample: &Sample) {
        match self {
            Pipeline::Bare { per_series } => {
                // Bare passthrough preserves the full label
                // set including `__name__` — vec/vec set ops
                // and binary ops downstream rely on the full
                // identity to match correctly. (Window /
                // Aggregate pipelines deliberately drop
                // `__name__` because their output represents
                // a new series identity; selectors don't.)
                let key = canonical_labels(labels);
                per_series.entry(key).or_default().push(*sample);
            }
            Pipeline::Aggregate {
                reducer,
                grouping,
                groups,
            } => {
                let key = group_key(labels, grouping);
                let ts_map = groups.entry(key).or_default();
                let acc = ts_map
                    .entry(sample.timestamp_ms)
                    .or_insert_with(|| reducer.empty());
                reducer.ingest(acc, sample);
            }
            Pipeline::Window {
                reducer,
                per_series,
                ..
            } => {
                let key = series_key(labels);
                let acc = per_series.entry(key).or_insert_with(|| reducer.empty());
                reducer.ingest(acc, sample);
            }
            Pipeline::WindowedAggregate {
                window_reducer,
                per_series,
                ..
            } => {
                let key = series_key(labels);
                let acc = per_series
                    .entry(key)
                    .or_insert_with(|| window_reducer.empty());
                window_reducer.ingest(acc, sample);
            }
        }
    }
}

impl Node {
    fn ingest(&mut self, labels: &[(String, String)], sample: &Sample) {
        match self {
            Node::Leaf(leaf) => {
                if matches_against(&leaf.matchers, labels) {
                    leaf.pipeline.ingest(labels, sample);
                }
            }
            Node::Binary { left, right, .. } => {
                left.ingest(labels, sample);
                right.ingest(labels, sample);
            }
        }
    }

    fn reset(&mut self) {
        match self {
            Node::Leaf(leaf) => leaf.pipeline.reset(),
            Node::Binary { left, right, .. } => {
                left.reset();
                right.reset();
            }
        }
    }
}

impl Pipeline {
    fn reset(&mut self) {
        match self {
            Pipeline::Bare { per_series } => per_series.clear(),
            Pipeline::Aggregate { groups, .. } => groups.clear(),
            Pipeline::Window { per_series, .. } => per_series.clear(),
            Pipeline::WindowedAggregate { per_series, .. } => per_series.clear(),
        }
    }
}

/// Matcher predicate over a label set — same shape as the
/// eval-side check. Used by leaf-node ingest to filter
/// samples whose labels don't satisfy the leaf's matchers.
fn matches_against(matchers: &[Matcher], labels: &[(String, String)]) -> bool {
    matchers.iter().all(|m| {
        let v = labels
            .iter()
            .find(|(k, _)| k == &m.label)
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        match m.op {
            MatcherOp::Eq => v == m.value,
            MatcherOp::Ne => v != m.value,
            MatcherOp::EqRegex | MatcherOp::NeRegex => v == m.value,
        }
    })
}

impl StreamingPlan {
    /// Ingest one sample identified by its label set. Samples
    /// whose labels don't satisfy any leaf's matchers are
    /// silently dropped at that leaf — vec/vec binary plans
    /// route the same sample to both children, each with its
    /// own matcher set.
    pub fn ingest_sample(&mut self, labels: &[(String, String)], sample: &Sample) {
        self.root.ingest(labels, sample);
    }

    /// Convenience batch form — ingest a full series
    /// (one label set, many samples).
    pub fn ingest_series(&mut self, labels: &[(String, String)], samples: &[Sample]) {
        for s in samples {
            self.ingest_sample(labels, s);
        }
    }

    /// Convenience: ingest a slice of `Series`. Each series's
    /// `labels` and `samples` flow through the per-sample
    /// path; non-matching series are dropped.
    pub fn ingest(&mut self, series: &[Series]) {
        for s in series {
            self.ingest_series(&s.labels, &s.samples);
        }
    }

    /// Read the current result. Each output series carries a
    /// single sample at `anchor_ms` whose value is the
    /// reducer's `snapshot()`. Groups whose accumulator is
    /// empty (no contributing samples) emit NaN, matching
    /// the eval-side rollup semantics (`first_over_time`,
    /// `sum`, etc. with empty input).
    ///
    /// Scalar postprocesses (from `<vec> <op> <scalar>` and
    /// the like) run in compile order on every emitted
    /// sample value. NaN values pass through unchanged when
    /// the op would propagate NaN; comparison ops with the
    /// `bool` modifier produce 0/1.
    pub fn snapshot(&self, anchor_ms: i64) -> Vec<Series> {
        self.root.snapshot(anchor_ms)
    }

    /// Reset every accumulator in the plan back to identity.
    /// Used by tumbling-window cadences that want a clean
    /// slate at each grid tick.
    pub fn reset(&mut self) {
        self.root.reset();
    }

    /// Collect every leaf's matcher set for the runtime to
    /// drive ingest with. Each leaf has its own matchers
    /// (vec/vec plans select two different metric names);
    /// the runtime fetches each set independently and feeds
    /// the results back to the plan via `ingest_series`. The
    /// plan's per-leaf matcher checks then route samples to
    /// the right leaf.
    pub fn leaf_matchers(&self) -> Vec<Vec<Matcher>> {
        let mut out = Vec::new();
        self.root.collect_matchers(&mut out);
        out
    }
}

impl Node {
    fn collect_matchers(&self, out: &mut Vec<Vec<Matcher>>) {
        match self {
            Node::Leaf(leaf) => out.push(leaf.matchers.clone()),
            Node::Binary { left, right, .. } => {
                left.collect_matchers(out);
                right.collect_matchers(out);
            }
        }
    }
}

impl Pipeline {
    fn snapshot(&self, anchor_ms: i64) -> Vec<Series> {
        match self {
            Pipeline::Bare { per_series } => per_series
                .iter()
                .map(|(labels, samples)| Series {
                    labels: labels.clone(),
                    samples: samples.clone(),
                })
                .collect(),
            Pipeline::Aggregate {
                reducer, groups, ..
            } => {
                let mut out = Vec::with_capacity(groups.len());
                for (labels, ts_map) in groups {
                    let samples: Vec<Sample> = ts_map
                        .iter()
                        .map(|(ts, acc)| Sample {
                            timestamp_ms: *ts,
                            value: reducer.snapshot(acc),
                        })
                        .collect();
                    out.push(Series {
                        labels: labels.clone(),
                        samples,
                    });
                }
                let _ = anchor_ms;
                out
            }
            Pipeline::Window {
                reducer,
                per_series,
                ..
            } => {
                let mut out = Vec::with_capacity(per_series.len());
                for (labels, acc) in per_series {
                    let value = reducer.snapshot(acc);
                    out.push(Series {
                        labels: labels.clone(),
                        samples: vec![Sample {
                            timestamp_ms: anchor_ms,
                            value,
                        }],
                    });
                }
                out
            }
            Pipeline::WindowedAggregate {
                window_reducer,
                per_series,
                outer_reducer,
                grouping,
                ..
            } => {
                let mut groups: BTreeMap<Vec<(String, String)>, AccCell> = BTreeMap::new();
                for (labels, w_acc) in per_series {
                    let inner_value = window_reducer.snapshot(w_acc);
                    if inner_value.is_nan() {
                        continue;
                    }
                    let key = group_key(labels, grouping);
                    let entry = groups.entry(key).or_insert_with(|| outer_reducer.empty());
                    outer_reducer.ingest(
                        entry,
                        &Sample {
                            timestamp_ms: anchor_ms,
                            value: inner_value,
                        },
                    );
                }
                let mut out = Vec::with_capacity(groups.len());
                for (labels, acc) in groups {
                    let value = outer_reducer.snapshot(&acc);
                    out.push(Series {
                        labels,
                        samples: vec![Sample {
                            timestamp_ms: anchor_ms,
                            value,
                        }],
                    });
                }
                out
            }
        }
    }
}

impl Node {
    fn snapshot(&self, anchor_ms: i64) -> Vec<Series> {
        match self {
            Node::Leaf(leaf) => {
                let mut out = leaf.pipeline.snapshot(anchor_ms);
                if !leaf.scalar_postprocesses.is_empty() {
                    for series in out.iter_mut() {
                        for sample in series.samples.iter_mut() {
                            for post in &leaf.scalar_postprocesses {
                                sample.value = apply_scalar_post(*post, sample.value);
                            }
                        }
                    }
                }
                out
            }
            Node::Binary {
                op,
                bool_modifier,
                group_modifier,
                join_modifier,
                left,
                right,
            } => {
                let l = left.snapshot(anchor_ms);
                let r = right.snapshot(anchor_ms);
                combine_binary_streaming(
                    *op,
                    *bool_modifier,
                    group_modifier.as_ref(),
                    join_modifier.as_ref(),
                    &l,
                    &r,
                )
            }
        }
    }
}

/// Vec/vec binary-op snapshot combine. Reuses the eval-side
/// `combine_vectors_modified` so streaming and batch agree
/// on matching key derivation, label projection, and
/// cardinality (`group_left`/`group_right`).
///
/// For comparison ops in filter mode (no `bool` modifier),
/// pairs whose predicate is false produce NaN — the eval
/// path's `prune_nan_samples` step is replicated here so
/// the streaming output mirrors batch.
fn combine_binary_streaming(
    op: BinaryOp,
    bool_modifier: bool,
    group_modifier: Option<&crate::ast::GroupModifier>,
    join_modifier: Option<&crate::ast::JoinModifier>,
    left: &[Series],
    right: &[Series],
) -> Vec<Series> {
    use crate::ast::BinaryOp::*;
    // Set ops (`and`/`or`/`unless`) reshape series and
    // sample lists rather than computing per-pair values.
    // Reuses the eval-side helper so streaming and batch
    // produce the same shape for these operators. Join
    // modifiers (group_left/right) aren't legal here —
    // matches the eval-side check.
    if matches!(op, And | Or | Unless) {
        if join_modifier.is_some() {
            // Caller-misuse — ignore the modifier and apply
            // the set op as if it weren't there. The streaming
            // compiler stage doesn't reject this today; could
            // tighten later.
        }
        return crate::eval::combine_set_op(op, left, right, group_modifier);
    }
    let combine = |l: f64, r: f64| crate::eval::eval_binary_value(op, l, r, bool_modifier);
    let mut out =
        crate::eval::combine_vectors_modified(left, right, group_modifier, join_modifier, &combine);
    if crate::eval::is_cmp_op(op) && !bool_modifier {
        out = prune_nan_samples_streaming(out);
    }
    out
}

fn prune_nan_samples_streaming(input: Vec<Series>) -> Vec<Series> {
    input
        .into_iter()
        .filter_map(|s| {
            let kept: Vec<Sample> = s
                .samples
                .into_iter()
                .filter(|sm| !sm.value.is_nan())
                .collect();
            if kept.is_empty() {
                None
            } else {
                Some(Series {
                    labels: s.labels,
                    samples: kept,
                })
            }
        })
        .collect()
}

/// Apply a scalar postprocess to one sample value. Mirrors
/// the eval-side `eval_binary_value` semantics: NaN
/// propagates through arithmetic; comparison ops respect the
/// `bool` modifier (0/1) or produce NaN to mark "predicate
/// false" in filter mode (the streaming layer doesn't have
/// per-sample filtering, so filter-mode false collapses to
/// NaN — pruning is a caller concern).
fn apply_scalar_post(post: ScalarPost, v: f64) -> f64 {
    use BinaryOp::*;
    let (l, r) = if post.scalar_on_left {
        (post.scalar, v)
    } else {
        (v, post.scalar)
    };
    match post.op {
        Add => l + r,
        Sub => l - r,
        Mul => l * r,
        Div => l / r,
        Mod => l % r,
        Pow => {
            if l.is_nan() {
                f64::NAN
            } else {
                l.powf(r)
            }
        }
        Atan2 => l.atan2(r),
        Eq | Ne | Lt | Le | Gt | Ge => {
            let cmp = match post.op {
                Eq => bin_eq_post(l, r),
                Ne => bin_neq_post(l, r),
                Gt => l > r,
                Lt => l < r,
                Ge => l >= r,
                Le => l <= r,
                _ => unreachable!(),
            };
            if post.bool_modifier {
                if cmp { 1.0 } else { 0.0 }
            } else if cmp {
                l
            } else {
                f64::NAN
            }
        }
        Default => {
            if l.is_nan() {
                r
            } else {
                l
            }
        }
        If => {
            if r.is_nan() {
                f64::NAN
            } else {
                l
            }
        }
        IfNot => {
            if r.is_nan() {
                l
            } else {
                f64::NAN
            }
        }
        And | Or | Unless => f64::NAN, // set ops are vec/vec, never reach here
    }
}

fn bin_eq_post(l: f64, r: f64) -> bool {
    if l.is_nan() {
        return r.is_nan();
    }
    l == r
}

fn bin_neq_post(l: f64, r: f64) -> bool {
    if l.is_nan() {
        return !r.is_nan();
    }
    if r.is_nan() {
        return true;
    }
    l != r
}

/// Sorted copy of `labels` — keeps the full identity
/// (including `__name__`). Used by the `Bare` pipeline
/// where selector output must preserve `__name__` for
/// downstream binary / set-op matching.
fn canonical_labels(labels: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<_> = labels.to_vec();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Series identity for windowed reducers: every label except
/// `__name__`, sorted by key for stable hashing across
/// arrival orders.
fn series_key(labels: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<_> = labels
        .iter()
        .filter(|(k, _)| k != "__name__")
        .cloned()
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Group key derivation per the active aggregate modifier.
/// Mirrors `eval::group_key` semantics: `__name__` is
/// dropped under `Without`; `By` keeps only the listed
/// labels; `All` collapses everything to the empty key.
fn group_key(labels: &[(String, String)], grouping: &Grouping) -> Vec<(String, String)> {
    let mut out: Vec<_> = match grouping {
        Grouping::All => Vec::new(),
        Grouping::By(keep) => labels
            .iter()
            .filter(|(k, _)| keep.iter().any(|w| w == k))
            .cloned()
            .collect(),
        Grouping::Without(drop) => labels
            .iter()
            .filter(|(k, _)| k != "__name__" && !drop.iter().any(|w| w == k))
            .cloned()
            .collect(),
    };
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Compile a parsed metricsql expression into a streaming
/// plan. Supported shapes (see SRD-47 §"Plan compilation"):
///
/// - `<agg>(<selector>) [by|without (...)]`
/// - `<rollup_fn>(<selector>[w])`
/// - `<agg>(<rollup_fn>(<selector>[w])) [by|without (...)]`
///
/// `<agg>` ∈ `{sum, count, min, max, group}`,
/// `<rollup_fn>` ∈ `{sum/count/min/max/first/last_over_time}`.
/// Any other AST shape returns
/// [`CompileError::Unsupported`].
pub fn compile_streaming(expr: &Expr) -> Result<StreamingPlan, CompileError> {
    let root = compile_node(expr)?;
    Ok(StreamingPlan { root })
}

/// Compile any AST node into the streaming `Node` tree.
///
/// - **Scalar/vector binary ops** fold into a postprocess
///   chain on the underlying leaf — `rate(...) * 100`
///   produces one leaf with `Mul 100` on its
///   `scalar_postprocesses`.
/// - **Vector/vector binary ops** produce `Node::Binary`
///   wrapping two recursively-compiled children. Matching
///   modifiers (`on`/`ignoring`/`group_left`/`group_right`)
///   ride on the binary node and apply at snapshot time.
fn compile_node(expr: &Expr) -> Result<Node, CompileError> {
    if let Expr::Binary(b) = expr {
        // Scalar/vector — descend into the vector side and
        // append a postprocess. Multi-level chains
        // (`(rate * 100) / 60`) walk recursively.
        if let Some(scalar) = numeric_value(&b.left) {
            let mut inner = compile_node(&b.right)?;
            push_scalar_postprocess(
                &mut inner,
                ScalarPost {
                    op: b.op,
                    scalar,
                    scalar_on_left: true,
                    bool_modifier: b.bool_modifier,
                },
            )?;
            return Ok(inner);
        }
        if let Some(scalar) = numeric_value(&b.right) {
            let mut inner = compile_node(&b.left)?;
            push_scalar_postprocess(
                &mut inner,
                ScalarPost {
                    op: b.op,
                    scalar,
                    scalar_on_left: false,
                    bool_modifier: b.bool_modifier,
                },
            )?;
            return Ok(inner);
        }
        // Vector/vector.
        let left = Box::new(compile_node(&b.left)?);
        let right = Box::new(compile_node(&b.right)?);
        return Ok(Node::Binary {
            op: b.op,
            bool_modifier: b.bool_modifier,
            group_modifier: b.group_modifier.clone(),
            join_modifier: b.join_modifier.clone(),
            left,
            right,
        });
    }
    compile_leaf_node(expr)
}

/// Append a scalar postprocess to whatever pipeline lives
/// at the bottom of `node`. For binary nodes we'd need a
/// dedicated wrapper (e.g. `(a + b) * 2` → outer postproc
/// over the combined result). To keep this push contained,
/// reject scalar postprocesses on top of binary nodes —
/// users can rewrite as `(a * 2) + (b * 2)`.
fn push_scalar_postprocess(node: &mut Node, post: ScalarPost) -> Result<(), CompileError> {
    match node {
        Node::Leaf(leaf) => {
            leaf.scalar_postprocesses.push(post);
            Ok(())
        }
        Node::Binary { .. } => Err(CompileError::Unsupported(
            "scalar binary op over a vector/vector expression — rewrite to push the scalar onto each side instead".into())),
    }
}

/// Compile a non-binary expression to a `Node::Leaf`.
fn compile_leaf_node(expr: &Expr) -> Result<Node, CompileError> {
    match expr {
        Expr::Func(f) => compile_func_leaf(f),
        Expr::Metric(m) => Ok(Node::Leaf(LeafState {
            matchers: matchers_for(m)?,
            pipeline: Pipeline::Bare {
                per_series: BTreeMap::new(),
            },
            scalar_postprocesses: Vec::new(),
        })),
        other => Err(CompileError::Unsupported(format!(
            "leaf expression {:?} not supported in streaming plans",
            short_kind(other)
        ))),
    }
}

/// Walk binary ops with scalar operands as a postprocess
/// chain. Returns `(vector_plan, postprocess_chain)` where
/// the chain is applied in iteration order at snapshot time.
/// Vector/vector binary ops (both sides have matchers) are
/// rejected here — they require a recursive plan tree
/// that lives in a future push.
/// Extract a numeric scalar from an expression. Constant
/// folding has already collapsed pure-arithmetic literals at
/// parse time, so the surviving `Number` nodes here are
/// either bare literals or fold-survivor placeholders.
fn numeric_value(expr: &Expr) -> Option<f64> {
    if let Expr::Number(NumberExpr { value, .. }) = expr {
        Some(*value)
    } else {
        None
    }
}

fn compile_func_leaf(f: &FuncExpr) -> Result<Node, CompileError> {
    // `quantile_over_time(phi, range_vec)` is the only
    // 2-arg streaming function. Lift it out before the
    // arity check so the rest of the dispatch stays focused
    // on the 1-arg shape.
    if f.name.eq_ignore_ascii_case("quantile_over_time") {
        return compile_quantile_over_time_leaf(f);
    }
    if f.args.len() != 1 {
        return Err(CompileError::InvalidShape(format!(
            "streaming function {:?} expects 1 argument, got {}",
            f.name,
            f.args.len()
        )));
    }
    let outer_kind = AggKind::classify(&f.name);
    let inner = &f.args[0];
    match outer_kind {
        AggKind::Aggregate(outer_reducer) => match inner {
            Expr::Metric(m) => Ok(leaf(
                matchers_for(m)?,
                Pipeline::Aggregate {
                    reducer: outer_reducer,
                    grouping: grouping_for(f.modifier.as_ref()),
                    groups: BTreeMap::new(),
                },
            )),
            Expr::Func(inner_f) => {
                let AggKind::Window(inner_reducer) = AggKind::classify(&inner_f.name) else {
                    return Err(CompileError::Unsupported(format!(
                        "aggregate {:?} cannot wrap non-rollup function {:?}",
                        f.name, inner_f.name
                    )));
                };
                if inner_f.args.len() != 1 {
                    return Err(CompileError::InvalidShape(format!(
                        "rollup function {:?} expects 1 argument",
                        inner_f.name
                    )));
                }
                let (matchers, window_ms) = matchers_and_window(&inner_f.args[0])?;
                let inner_reducer = inner_reducer.bind_window(window_ms);
                Ok(leaf(
                    matchers,
                    Pipeline::WindowedAggregate {
                        window_reducer: inner_reducer,
                        window_ms,
                        per_series: BTreeMap::new(),
                        outer_reducer,
                        grouping: grouping_for(f.modifier.as_ref()),
                    },
                ))
            }
            other => Err(CompileError::Unsupported(format!(
                "aggregate {:?} over {:?} not supported in streaming plans",
                f.name,
                short_kind(other)
            ))),
        },
        AggKind::Window(window_reducer) => {
            let (matchers, window_ms) = matchers_and_window(inner)?;
            let reducer = window_reducer.bind_window(window_ms);
            Ok(leaf(
                matchers,
                Pipeline::Window {
                    reducer,
                    window_ms,
                    per_series: BTreeMap::new(),
                },
            ))
        }
        AggKind::Unsupported => Err(CompileError::Unsupported(format!(
            "function {:?} not supported in streaming plans this push",
            f.name
        ))),
    }
}

/// Build a leaf node with empty postprocess chain.
fn leaf(matchers: Vec<Matcher>, pipeline: Pipeline) -> Node {
    Node::Leaf(LeafState {
        matchers,
        pipeline,
        scalar_postprocesses: Vec::new(),
    })
}

/// `quantile_over_time(phi, range_vec)` — 2-arg form. The
/// quantile is a numeric literal; the second arg is a
/// rollup expression `<selector>[<window>]`. Compiles to a
/// per-series `Window` pipeline carrying the quantile in
/// the reducer kind.
fn compile_quantile_over_time_leaf(f: &FuncExpr) -> Result<Node, CompileError> {
    if f.args.len() != 2 {
        return Err(CompileError::InvalidShape(format!(
            "quantile_over_time expects 2 args (phi, range_vec), got {}",
            f.args.len()
        )));
    }
    let Some(quantile) = numeric_value(&f.args[0]) else {
        return Err(CompileError::InvalidShape(
            "quantile_over_time first arg must be a numeric quantile (0..=1)".into(),
        ));
    };
    if !(0.0..=1.0).contains(&quantile) {
        return Err(CompileError::InvalidShape(format!(
            "quantile_over_time phi must be in [0, 1], got {quantile}"
        )));
    }
    let (matchers, window_ms) = matchers_and_window(&f.args[1])?;
    Ok(leaf(
        matchers,
        Pipeline::Window {
            reducer: ReducerKind::QuantileOverTime { quantile },
            window_ms,
            per_series: BTreeMap::new(),
        },
    ))
}

/// Classification of a function name into aggregate vs.
/// rollup vs. unsupported. Internal to the compiler.
enum AggKind {
    /// Cross-series aggregate (top-level).
    Aggregate(ReducerKind),
    /// Per-series window reducer (`*_over_time`).
    Window(ReducerKind),
    /// Algebraic / holistic / unsupported function name.
    Unsupported,
}

impl AggKind {
    fn classify(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            // Distributive cross-series aggregates.
            "sum" => Self::Aggregate(ReducerKind::Sum),
            "count" => Self::Aggregate(ReducerKind::Count),
            "min" => Self::Aggregate(ReducerKind::Min),
            "max" => Self::Aggregate(ReducerKind::Max),
            "group" => Self::Aggregate(ReducerKind::Group),
            // Algebraic cross-series aggregates.
            "avg" => Self::Aggregate(ReducerKind::Avg),
            "stddev" => Self::Aggregate(ReducerKind::Stddev),
            "stdvar" => Self::Aggregate(ReducerKind::Stdvar),
            // Distributive per-series rollups.
            "sum_over_time" => Self::Window(ReducerKind::Sum),
            "count_over_time" => Self::Window(ReducerKind::Count),
            "min_over_time" => Self::Window(ReducerKind::Min),
            "max_over_time" => Self::Window(ReducerKind::Max),
            "first_over_time" => Self::Window(ReducerKind::FirstOverTime),
            "last_over_time" => Self::Window(ReducerKind::LastOverTime),
            // Algebraic per-series rollups.
            "avg_over_time" => Self::Window(ReducerKind::Avg),
            "stddev_over_time" => Self::Window(ReducerKind::Stddev),
            "stdvar_over_time" => Self::Window(ReducerKind::Stdvar),
            // Algebraic counter / gauge rollups.
            "increase" => Self::Window(ReducerKind::Increase),
            "delta" => Self::Window(ReducerKind::Delta),
            // Rate's window is bound at compile-time (see
            // `compile_func`); placeholder `0` here is
            // overwritten before the kind reaches the plan.
            "rate" => Self::Window(ReducerKind::Rate { window_ms: 0 }),
            _ => Self::Unsupported,
        }
    }
}

fn grouping_for(modifier: Option<&AggrModifier>) -> Grouping {
    let Some(m) = modifier else {
        return Grouping::All;
    };
    match m.op {
        AggrModifierOp::By => Grouping::By(m.args.clone()),
        AggrModifierOp::Without => Grouping::Without(m.args.clone()),
    }
}

fn matchers_for(m: &MetricExpr) -> Result<Vec<Matcher>, CompileError> {
    use crate::ast::LabelFilterOp;
    if m.label_filterss.len() > 1 {
        return Err(CompileError::Unsupported(
            "selectors with `or` filter groups not supported in streaming plans this push".into(),
        ));
    }
    let group = m.label_filterss.first().cloned().unwrap_or_default();
    let mut out = Vec::with_capacity(group.len());
    for lf in group {
        if lf.is_template_ref || lf.value_expr.is_some() {
            return Err(CompileError::InvalidShape(format!(
                "unexpanded WITH template ref on filter {:?} — call `parse` not `parse_for_prettify`",
                lf.label
            )));
        }
        out.push(Matcher {
            label: lf.label,
            op: match lf.op {
                LabelFilterOp::Eq => MatcherOp::Eq,
                LabelFilterOp::Ne => MatcherOp::Ne,
                LabelFilterOp::EqRegex => MatcherOp::EqRegex,
                LabelFilterOp::NeRegex => MatcherOp::NeRegex,
            },
            value: lf.value,
        });
    }
    Ok(out)
}

/// Accept `<selector>[<window>]` (a `RollupExpr` wrapping a
/// `MetricExpr`) and return the matchers + window in
/// milliseconds. Rejects rollups without a window — the
/// streaming layer doesn't infer step-relative windows.
fn matchers_and_window(arg: &Expr) -> Result<(Vec<Matcher>, i64), CompileError> {
    let Expr::Rollup(re) = arg else {
        return Err(CompileError::InvalidShape(
            "rollup function argument must be `<selector>[<window>]`".into(),
        ));
    };
    let Some(win) = &re.window else {
        return Err(CompileError::InvalidShape(
            "rollup function requires an explicit `[<window>]` argument".into(),
        ));
    };
    let window_ms = parse_window_ms(&win.value).map_err(|e| {
        CompileError::InvalidShape(format!("bad rollup window {:?}: {e}", win.value))
    })?;
    let inner = re_inner_metric(re)?;
    let matchers = matchers_for(inner)?;
    Ok((matchers, window_ms))
}

fn re_inner_metric(re: &RollupExpr) -> Result<&MetricExpr, CompileError> {
    match &*re.expr {
        Expr::Metric(m) => Ok(m),
        other => Err(CompileError::Unsupported(format!(
            "rollup over {:?} (expected bare selector)",
            short_kind(other)
        ))),
    }
}

/// Minimal duration parser for plan compilation. Supports
/// the units the streaming layer's tests exercise; for the
/// full unit set the eval path's parser handles, see
/// `eval::parse_duration_ms`.
fn parse_window_ms(s: &str) -> Result<i64, String> {
    let mut total: i64 = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        if i == start {
            return Err("expected number".into());
        }
        let n: f64 = s[start..i]
            .parse()
            .map_err(|e| format!("number parse: {e}"))?;
        let unit_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let unit = &s[unit_start..i];
        let mult = match unit {
            "ms" => 1.0,
            "s" => 1_000.0,
            "m" => 60_000.0,
            "h" => 3_600_000.0,
            "d" => 86_400_000.0,
            "" => return Err("missing unit".into()),
            other => return Err(format!("unknown unit {other:?}")),
        };
        total = total.saturating_add((n * mult) as i64);
    }
    Ok(total)
}

fn short_kind(e: &Expr) -> &'static str {
    match e {
        Expr::Metric(_) => "Metric",
        Expr::Number(_) => "Number",
        Expr::String(_) => "String",
        Expr::Duration(_) => "Duration",
        Expr::Func(_) => "Func",
        Expr::Binary(_) => "Binary",
        Expr::Rollup(_) => "Rollup",
        Expr::Paren(_) => "Paren",
        Expr::With(_) => "With",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(t: i64, v: f64) -> Sample {
        Sample {
            timestamp_ms: t,
            value: v,
        }
    }

    /// Build an accumulator from a slice of samples by
    /// `ingest`-ing them in order. Used by the monoid-law
    /// tests as a one-step "make me an accumulator" helper.
    fn build<R: Reducer>(r: &R, samples: &[Sample]) -> R::Acc {
        let mut acc = R::Acc::default();
        for sm in samples {
            r.ingest(&mut acc, sm);
        }
        acc
    }

    /// Verify the three monoid laws for one reducer against
    /// a fixed set of inputs. Each test below picks inputs
    /// that exercise distinct accumulator regions (with /
    /// without data, NaN handling, edge values).
    fn assert_monoid_laws<R: Reducer>(r: &R, a: &[Sample], b: &[Sample], c: &[Sample]) {
        // identity: merge(empty, x) ≡ x
        let x = build(r, a);
        let mut empty = R::Acc::default();
        let x_clone = x.clone();
        r.merge(&mut empty, x);
        assert_snap_eq(r, &empty, &x_clone, "identity (empty merged into)");

        let mut x_owned = x_clone.clone();
        let empty2 = R::Acc::default();
        r.merge(&mut x_owned, empty2);
        assert_snap_eq(r, &x_owned, &x_clone, "identity (empty merged onto)");

        // commutative: merge(a, b) ≡ merge(b, a)
        let mut ab = build(r, a);
        let bcc = build(r, b);
        r.merge(&mut ab, bcc);
        let mut ba = build(r, b);
        let acc2 = build(r, a);
        r.merge(&mut ba, acc2);
        assert_snap_eq(r, &ab, &ba, "commutativity");

        // associative: merge(merge(a, b), c) ≡ merge(a, merge(b, c))
        let mut left = build(r, a);
        let bcc2 = build(r, b);
        r.merge(&mut left, bcc2);
        let cleft = build(r, c);
        r.merge(&mut left, cleft);

        let mut right_inner = build(r, b);
        let c_inner = build(r, c);
        r.merge(&mut right_inner, c_inner);
        let mut right = build(r, a);
        r.merge(&mut right, right_inner);

        assert_snap_eq(r, &left, &right, "associativity");
    }

    fn assert_snap_eq<R: Reducer>(r: &R, x: &R::Acc, y: &R::Acc, why: &str) {
        let xv = r.snapshot(x);
        let yv = r.snapshot(y);
        // NaN-aware equality so identity-on-empty doesn't
        // fail the test (NaN != NaN normally).
        if xv.is_nan() && yv.is_nan() {
            return;
        }
        let tol = 1e-9_f64.max(xv.abs() * 1e-12);
        assert!(
            (xv - yv).abs() <= tol,
            "{why}: snapshots diverged: {xv} vs {yv}"
        );
    }

    #[test]
    fn monoid_laws_sum() {
        let a = [s(0, 1.0), s(1, 2.5), s(2, -0.5)];
        let b = [s(3, 100.0)];
        let c = [s(4, 0.0), s(5, f64::NAN), s(6, 7.7)];
        assert_monoid_laws(&SumReducer, &a, &b, &c);
        // Empty snapshot is NaN per `has_data == false`.
        assert!(SumReducer.snapshot(&KahanAcc::default()).is_nan());
        // Non-empty snapshot is finite.
        assert_eq!(SumReducer.snapshot(&build(&SumReducer, &a)), 3.0);
    }

    #[test]
    fn monoid_laws_count() {
        let a = [s(0, 1.0), s(1, 2.0)];
        let b = [s(2, 3.0), s(3, f64::NAN)];
        let c = [s(4, 5.0)];
        assert_monoid_laws(&CountReducer, &a, &b, &c);
        // Count's empty is 0.0 not NaN — distinguishes
        // count from sum/min/max snapshots.
        assert_eq!(CountReducer.snapshot(&CountAcc::default()), 0.0);
        assert_eq!(CountReducer.snapshot(&build(&CountReducer, &a)), 2.0);
        // NaN samples skipped.
        assert_eq!(CountReducer.snapshot(&build(&CountReducer, &b)), 1.0);
    }

    #[test]
    fn monoid_laws_min() {
        let a = [s(0, 5.0), s(1, 3.0)];
        let b = [s(2, 7.0)];
        let c = [s(3, -1.0), s(4, f64::NAN)];
        assert_monoid_laws(&MinReducer, &a, &b, &c);
        assert!(MinReducer.snapshot(&MinAcc::default()).is_nan());
        assert_eq!(MinReducer.snapshot(&build(&MinReducer, &a)), 3.0);
        // NaN doesn't replace min.
        assert_eq!(MinReducer.snapshot(&build(&MinReducer, &c)), -1.0);
    }

    #[test]
    fn monoid_laws_max() {
        let a = [s(0, 5.0), s(1, 3.0)];
        let b = [s(2, 7.0)];
        let c = [s(3, -1.0), s(4, f64::NAN)];
        assert_monoid_laws(&MaxReducer, &a, &b, &c);
        assert!(MaxReducer.snapshot(&MaxAcc::default()).is_nan());
        assert_eq!(MaxReducer.snapshot(&build(&MaxReducer, &a)), 5.0);
        assert_eq!(MaxReducer.snapshot(&build(&MaxReducer, &b)), 7.0);
    }

    #[test]
    fn monoid_laws_group() {
        let a = [s(0, 1.0)];
        let b = [s(1, f64::NAN)];
        let c = [];
        assert_monoid_laws(&GroupReducer, &a, &b, &c);
        assert!(GroupReducer.snapshot(&GroupAcc::default()).is_nan());
        assert_eq!(GroupReducer.snapshot(&build(&GroupReducer, &a)), 1.0);
        // NaN-only inputs leave `has_data` false.
        assert!(GroupReducer.snapshot(&build(&GroupReducer, &b)).is_nan());
    }

    #[test]
    fn monoid_laws_first_over_time() {
        let a = [s(10, 1.0), s(20, 2.0)];
        let b = [s(5, 99.0)];
        let c = [s(15, 3.0), s(25, f64::NAN)];
        assert_monoid_laws(&FirstOverTimeReducer, &a, &b, &c);
        assert!(FirstOverTimeReducer.snapshot(&FirstAcc::default()).is_nan());
        // First sample is the one with the smallest timestamp,
        // regardless of ingest order.
        let acc = build(&FirstOverTimeReducer, &a);
        assert_eq!(FirstOverTimeReducer.snapshot(&acc), 1.0);
    }

    #[test]
    fn first_over_time_picks_smallest_timestamp_across_partitions() {
        let r = FirstOverTimeReducer;
        // Ingest order shouldn't matter — the acc with the
        // smaller timestamp wins on merge.
        let mut a = FirstAcc::default();
        r.ingest(&mut a, &s(20, 2.0));
        let mut b = FirstAcc::default();
        r.ingest(&mut b, &s(10, 1.0));
        r.merge(&mut a, b);
        assert_eq!(r.snapshot(&a), 1.0);
    }

    #[test]
    fn monoid_laws_last_over_time() {
        let a = [s(10, 1.0), s(20, 2.0)];
        let b = [s(30, 99.0)];
        let c = [s(15, 3.0), s(25, f64::NAN)];
        assert_monoid_laws(&LastOverTimeReducer, &a, &b, &c);
        assert!(LastOverTimeReducer.snapshot(&LastAcc::default()).is_nan());
        let acc = build(&LastOverTimeReducer, &a);
        assert_eq!(LastOverTimeReducer.snapshot(&acc), 2.0);
    }

    #[test]
    fn last_over_time_picks_largest_timestamp_across_partitions() {
        let r = LastOverTimeReducer;
        let mut a = LastAcc::default();
        r.ingest(&mut a, &s(10, 1.0));
        let mut b = LastAcc::default();
        r.ingest(&mut b, &s(20, 2.0));
        r.merge(&mut a, b);
        assert_eq!(r.snapshot(&a), 2.0);
    }

    // ---- compile_streaming shape tests ----

    use crate::parse;

    fn compile(q: &str) -> Result<StreamingPlan, CompileError> {
        let expr = parse(q).expect("parse");
        compile_streaming(&expr)
    }

    fn pipeline_kind(plan: &StreamingPlan) -> &'static str {
        match &plan.root {
            Node::Leaf(leaf) => match &leaf.pipeline {
                Pipeline::Bare { .. } => "Bare",
                Pipeline::Aggregate { .. } => "Aggregate",
                Pipeline::Window { .. } => "Window",
                Pipeline::WindowedAggregate { .. } => "WindowedAggregate",
            },
            Node::Binary { .. } => "Binary",
        }
    }

    /// Drill into a leaf's pipeline; tests use this when
    /// they want to match on the underlying pipeline shape.
    fn leaf_pipeline(plan: &StreamingPlan) -> &Pipeline {
        match &plan.root {
            Node::Leaf(leaf) => &leaf.pipeline,
            _ => panic!("expected Leaf root, got Binary"),
        }
    }

    /// Drill into a leaf's scalar postprocess chain.
    fn leaf_postprocesses(plan: &StreamingPlan) -> &[ScalarPost] {
        match &plan.root {
            Node::Leaf(leaf) => &leaf.scalar_postprocesses,
            _ => panic!("expected Leaf root, got Binary"),
        }
    }

    #[test]
    fn compile_aggregate_no_modifier() {
        let plan = compile("sum(cpu)").expect("compile");
        assert_eq!(pipeline_kind(&plan), "Aggregate");
        match leaf_pipeline(&plan) {
            Pipeline::Aggregate {
                reducer, grouping, ..
            } => {
                assert_eq!(*reducer, ReducerKind::Sum);
                assert!(matches!(grouping, Grouping::All));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn compile_aggregate_with_by_modifier() {
        let plan = compile("max(cpu) by (host, zone)").expect("compile");
        match leaf_pipeline(&plan) {
            Pipeline::Aggregate {
                reducer, grouping, ..
            } => {
                assert_eq!(*reducer, ReducerKind::Max);
                match grouping {
                    Grouping::By(labels) => assert_eq!(labels, &["host", "zone"]),
                    _ => panic!("expected By"),
                }
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn compile_aggregate_with_without_modifier() {
        let plan = compile("min(cpu) without (instance)").expect("compile");
        match leaf_pipeline(&plan) {
            Pipeline::Aggregate {
                reducer, grouping, ..
            } => {
                assert_eq!(*reducer, ReducerKind::Min);
                match grouping {
                    Grouping::Without(labels) => assert_eq!(labels, &["instance"]),
                    _ => panic!("expected Without"),
                }
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn compile_pure_window() {
        let plan = compile("max_over_time(cpu[5m])").expect("compile");
        match leaf_pipeline(&plan) {
            Pipeline::Window {
                reducer, window_ms, ..
            } => {
                assert_eq!(*reducer, ReducerKind::Max);
                assert_eq!(*window_ms, 5 * 60 * 1000);
            }
            _ => panic!("expected Window pipeline, got {:?}", pipeline_kind(&plan)),
        }
    }

    #[test]
    fn compile_windowed_aggregate() {
        let plan = compile("sum(sum_over_time(cpu[1m])) by (host)").expect("compile");
        match leaf_pipeline(&plan) {
            Pipeline::WindowedAggregate {
                window_reducer,
                outer_reducer,
                window_ms,
                grouping,
                ..
            } => {
                assert_eq!(*window_reducer, ReducerKind::Sum);
                assert_eq!(*outer_reducer, ReducerKind::Sum);
                assert_eq!(*window_ms, 60_000);
                match grouping {
                    Grouping::By(labels) => assert_eq!(labels, &["host"]),
                    _ => panic!("expected By"),
                }
            }
            _ => panic!(
                "expected WindowedAggregate pipeline, got {:?}",
                pipeline_kind(&plan)
            ),
        }
    }

    #[test]
    fn compile_first_last_over_time() {
        let plan = compile("first_over_time(cpu[5m])").expect("compile");
        match leaf_pipeline(&plan) {
            Pipeline::Window { reducer, .. } => {
                assert_eq!(*reducer, ReducerKind::FirstOverTime);
            }
            _ => unreachable!(),
        }
        let plan = compile("last_over_time(cpu[5m])").expect("compile");
        match leaf_pipeline(&plan) {
            Pipeline::Window { reducer, .. } => {
                assert_eq!(*reducer, ReducerKind::LastOverTime);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn compile_bare_selector() {
        let plan = compile("cpu").expect("compile");
        assert_eq!(pipeline_kind(&plan), "Bare");
    }

    // ---- rejection tests ----

    #[test]
    fn scalar_binary_compiles_with_postprocess_chain() {
        // `rate(cpu[1m]) * 100` — scalar on right.
        let plan = compile("rate(cpu[1m]) * 100").expect("compile");
        assert_eq!(leaf_postprocesses(&plan).len(), 1);
        assert!(matches!(leaf_postprocesses(&plan)[0].op, BinaryOp::Mul));
        assert!(!leaf_postprocesses(&plan)[0].scalar_on_left);
        assert_eq!(leaf_postprocesses(&plan)[0].scalar, 100.0);

        // `100 - cpu` — scalar on left.
        let plan = compile("100 - cpu").expect("compile");
        assert_eq!(leaf_postprocesses(&plan).len(), 1);
        assert!(leaf_postprocesses(&plan)[0].scalar_on_left);

        // Chained: `(cpu + 5) * 2` — postprocesses run in
        // source order (innermost first).
        let plan = compile("(cpu + 5) * 2").expect("compile");
        assert_eq!(leaf_postprocesses(&plan).len(), 2);
    }

    #[test]
    fn vector_vector_binary_compiles_to_node_tree() {
        let plan = compile("a + b").expect("compile");
        match &plan.root {
            Node::Binary {
                op, left, right, ..
            } => {
                assert!(matches!(op, BinaryOp::Add));
                assert!(matches!(**left, Node::Leaf(_)));
                assert!(matches!(**right, Node::Leaf(_)));
            }
            _ => panic!("expected Binary root, got {:?}", pipeline_kind(&plan)),
        }
    }

    #[test]
    fn vector_vector_binary_evaluates_per_pair() {
        let mut plan = compile("a + b").expect("compile");
        plan.ingest(&[
            series(&[("__name__", "a"), ("host", "h1")], &[(0, 1.0), (10, 2.0)]),
            series(
                &[("__name__", "b"), ("host", "h1")],
                &[(0, 10.0), (10, 20.0)],
            ),
            series(&[("__name__", "a"), ("host", "h2")], &[(0, 5.0)]),
        ]);
        let got = plan.snapshot(0);
        // h1: 1+10=11 at ts=0, 2+20=22 at ts=10.
        // h2 drops out — no match on right.
        assert_eq!(got.len(), 1);
        assert_eq!(lookup(&got[0], "host"), Some("h1"));
        let values: Vec<f64> = got[0].samples.iter().map(|s| s.value).collect();
        assert_eq!(values, vec![11.0, 22.0]);
        assert_eq!(lookup(&got[0], "__name__"), None);
    }

    #[test]
    fn vector_vector_binary_with_on_modifier() {
        let mut plan = compile("a * on(zone) b").expect("compile");
        plan.ingest(&[
            series(
                &[("__name__", "a"), ("zone", "z1"), ("host", "h1")],
                &[(0, 2.0)],
            ),
            series(
                &[("__name__", "b"), ("zone", "z1"), ("host", "h99")],
                &[(0, 3.0)],
            ),
        ]);
        let got = plan.snapshot(0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].samples[0].value, 6.0);
    }

    #[test]
    fn vector_vector_binary_with_group_left_carries_extras() {
        let mut plan = compile("a * on(zone) group_left(tier) b").expect("compile");
        plan.ingest(&[
            series(
                &[("__name__", "a"), ("zone", "z1"), ("host", "h1")],
                &[(0, 10.0)],
            ),
            series(
                &[("__name__", "a"), ("zone", "z1"), ("host", "h2")],
                &[(0, 20.0)],
            ),
            series(
                &[("__name__", "b"), ("zone", "z1"), ("tier", "prod")],
                &[(0, 2.0)],
            ),
        ]);
        let got = plan.snapshot(0);
        assert_eq!(got.len(), 2);
        for s in &got {
            assert_eq!(lookup(s, "tier"), Some("prod"));
            assert_eq!(lookup(s, "zone"), Some("z1"));
        }
    }

    #[test]
    fn vector_vector_set_op_and_keeps_left_present_on_right() {
        let mut plan = compile("a and b").expect("compile");
        plan.ingest(&[
            series(&[("__name__", "a"), ("host", "h1")], &[(0, 1.0)]),
            series(&[("__name__", "a"), ("host", "h2")], &[(0, 2.0)]),
            series(&[("__name__", "b"), ("host", "h1")], &[(0, 99.0)]),
        ]);
        let got = plan.snapshot(0);
        // Only h1 survives — h2 has no counterpart on b.
        assert_eq!(got.len(), 1);
        assert_eq!(lookup(&got[0], "host"), Some("h1"));
        // `and` keeps left's value, not right's.
        assert_eq!(got[0].samples[0].value, 1.0);
    }

    #[test]
    fn vector_vector_set_op_or_unions_disjoint_series() {
        let mut plan = compile("a or b").expect("compile");
        plan.ingest(&[
            series(&[("__name__", "a"), ("host", "h1")], &[(0, 1.0)]),
            series(&[("__name__", "b"), ("host", "h2")], &[(0, 2.0)]),
        ]);
        let got = plan.snapshot(0);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn vector_vector_set_op_unless_excludes_overlap() {
        let mut plan = compile("a unless b").expect("compile");
        plan.ingest(&[
            series(&[("__name__", "a"), ("host", "h1")], &[(0, 1.0)]),
            series(&[("__name__", "a"), ("host", "h2")], &[(0, 2.0)]),
            series(&[("__name__", "b"), ("host", "h1")], &[(0, 99.0)]),
        ]);
        let got = plan.snapshot(0);
        // h1 drops (matched on right), h2 survives.
        assert_eq!(got.len(), 1);
        assert_eq!(lookup(&got[0], "host"), Some("h2"));
    }

    #[test]
    fn scalar_postprocess_over_binary_is_rejected() {
        // `(a + b) * 2` — postprocess on a binary node
        // requires its own wrapper variant; deferred. The
        // user can rewrite as `(a * 2) + (b * 2)`.
        let err = compile("(a + b) * 2").expect_err("expected compile error");
        assert!(matches!(err, CompileError::Unsupported(_)));
    }

    #[test]
    fn algebraic_aggregate_compiles() {
        // Algebraic reducers landed in the follow-up push;
        // avg / stddev / stdvar should now compile to
        // Aggregate pipelines.
        for q in ["avg(cpu)", "stddev(cpu)", "stdvar(cpu)"] {
            let plan = compile(q).expect("compile");
            assert_eq!(pipeline_kind(&plan), "Aggregate", "query {q:?}");
        }
    }

    #[test]
    fn algebraic_rollup_compiles() {
        for q in [
            "rate(cpu[5m])",
            "increase(cpu[5m])",
            "delta(cpu[5m])",
            "avg_over_time(cpu[5m])",
            "stddev_over_time(cpu[5m])",
        ] {
            let plan = compile(q).expect("compile");
            assert_eq!(pipeline_kind(&plan), "Window", "query {q:?}");
        }
    }

    #[test]
    fn quantile_over_time_compiles_with_phi() {
        let plan = compile("quantile_over_time(0.99, cpu[5m])").expect("compile");
        match leaf_pipeline(&plan) {
            Pipeline::Window {
                reducer: ReducerKind::QuantileOverTime { quantile },
                window_ms,
                ..
            } => {
                assert_eq!(*quantile, 0.99);
                assert_eq!(*window_ms, 5 * 60 * 1000);
            }
            _ => panic!("expected Window with QuantileOverTime reducer"),
        }
    }

    #[test]
    fn quantile_over_time_rejects_invalid_phi() {
        for q in [
            "quantile_over_time(2.0, cpu[5m])",
            "quantile_over_time(-0.1, cpu[5m])",
        ] {
            let err = compile(q).expect_err("expected compile error");
            assert!(matches!(err, CompileError::InvalidShape(_)));
        }
    }

    #[test]
    fn quantile_over_time_ingest_picks_quantile() {
        // Hand-checked: HDR with sigfigs=3, values 1..=100,
        // q=0.5 should be ~50.
        let mut plan = compile("quantile_over_time(0.5, v[1m])").expect("compile");
        let labels: Vec<(String, String)> = vec![
            ("__name__".to_string(), "v".to_string()),
            ("host".to_string(), "h1".to_string()),
        ];
        for i in 1..=100i64 {
            plan.ingest_sample(
                &labels,
                &Sample {
                    timestamp_ms: i * 100,
                    value: i as f64,
                },
            );
        }
        let got = plan.snapshot(20_000);
        assert_eq!(got.len(), 1);
        let v = got[0].samples[0].value;
        assert!((v - 50.0).abs() <= 1.0, "expected p50 ≈ 50, got {v}");
    }

    #[test]
    fn rate_window_is_bound_at_compile_time() {
        let plan = compile("rate(cpu[3m])").expect("compile");
        match leaf_pipeline(&plan) {
            Pipeline::Window {
                reducer: ReducerKind::Rate { window_ms },
                ..
            } => {
                assert_eq!(*window_ms, 3 * 60 * 1000);
            }
            _ => panic!("expected Window with Rate reducer carrying 3m window"),
        }
    }

    #[test]
    fn reject_quantile_holistic() {
        // `quantile(0.9, cpu)` carries 2 args — the
        // arity check fires before classification, but the
        // result is still a clean rejection.
        let err = compile("quantile(0.9, cpu)").expect_err("expected compile error");
        assert!(matches!(
            err,
            CompileError::Unsupported(_) | CompileError::InvalidShape(_)
        ));
    }

    #[test]
    fn reject_or_filter_groups() {
        let err = compile(r#"cpu{host="a" or host="b"}"#).expect_err("expected compile error");
        assert!(matches!(err, CompileError::Unsupported(_)));
    }

    #[test]
    fn reject_rollup_without_window() {
        // Rate-style rollup without an explicit window —
        // rejected because the streaming layer doesn't infer
        // step-relative durations.
        let err = compile("sum_over_time(cpu)").expect_err("expected compile error");
        assert!(matches!(err, CompileError::InvalidShape(_)));
    }

    #[test]
    fn reject_top_level_number() {
        let err = compile("42").expect_err("expected compile error");
        assert!(matches!(err, CompileError::Unsupported(_)));
    }

    // ---- end-to-end ingest + snapshot tests ----

    fn series(labels: &[(&str, &str)], samples: &[(i64, f64)]) -> Series {
        Series {
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            samples: samples
                .iter()
                .map(|(t, v)| Sample {
                    timestamp_ms: *t,
                    value: *v,
                })
                .collect(),
        }
    }

    fn lookup<'a>(s: &'a Series, key: &str) -> Option<&'a str> {
        s.labels
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn ingest_aggregate_collapses_to_single_group_per_timestamp() {
        let mut plan = compile("sum(cpu)").expect("compile");
        // Two timestamps, three series → output should have
        // ONE group with TWO samples, one per timestamp.
        plan.ingest(&[
            series(
                &[("__name__", "cpu"), ("host", "a")],
                &[(0, 1.0), (10, 10.0)],
            ),
            series(
                &[("__name__", "cpu"), ("host", "b")],
                &[(0, 2.0), (10, 20.0)],
            ),
            series(
                &[("__name__", "cpu"), ("host", "c")],
                &[(0, 4.0), (10, 40.0)],
            ),
        ]);
        let got = plan.snapshot(0);
        assert_eq!(got.len(), 1);
        assert!(got[0].labels.is_empty());
        assert_eq!(got[0].samples.len(), 2);
        // T=0: 1+2+4 = 7
        assert_eq!(got[0].samples[0].timestamp_ms, 0);
        assert_eq!(got[0].samples[0].value, 7.0);
        // T=10: 10+20+40 = 70
        assert_eq!(got[0].samples[1].timestamp_ms, 10);
        assert_eq!(got[0].samples[1].value, 70.0);
    }

    #[test]
    fn ingest_aggregate_by_groups_distinct_label_values() {
        let mut plan = compile("sum(cpu) by (zone)").expect("compile");
        plan.ingest(&[
            series(
                &[("__name__", "cpu"), ("zone", "z1"), ("host", "a")],
                &[(0, 1.0)],
            ),
            series(
                &[("__name__", "cpu"), ("zone", "z1"), ("host", "b")],
                &[(0, 2.0)],
            ),
            series(
                &[("__name__", "cpu"), ("zone", "z2"), ("host", "c")],
                &[(0, 4.0)],
            ),
        ]);
        let mut got = plan.snapshot(0);
        got.sort_by(|a, b| {
            lookup(a, "zone")
                .unwrap_or("")
                .cmp(lookup(b, "zone").unwrap_or(""))
        });
        assert_eq!(got.len(), 2);
        assert_eq!(lookup(&got[0], "zone"), Some("z1"));
        assert_eq!(got[0].samples[0].value, 3.0);
        assert_eq!(lookup(&got[1], "zone"), Some("z2"));
        assert_eq!(got[1].samples[0].value, 4.0);
    }

    #[test]
    fn ingest_window_holds_per_series_state() {
        let mut plan = compile("sum_over_time(cpu[1m])").expect("compile");
        plan.ingest(&[
            series(
                &[("__name__", "cpu"), ("host", "a")],
                &[(0, 1.0), (10, 2.0), (20, 4.0)],
            ),
            series(&[("__name__", "cpu"), ("host", "b")], &[(0, 8.0)]),
        ]);
        let mut got = plan.snapshot(100);
        got.sort_by(|a, b| {
            lookup(a, "host")
                .unwrap_or("")
                .cmp(lookup(b, "host").unwrap_or(""))
        });
        assert_eq!(got.len(), 2);
        // host=a: 1+2+4 = 7
        assert_eq!(got[0].samples[0].value, 7.0);
        // host=b: 8
        assert_eq!(got[1].samples[0].value, 8.0);
    }

    #[test]
    fn ingest_windowed_aggregate_two_stage_reduces() {
        let mut plan = compile("sum(sum_over_time(cpu[1m])) by (zone)").expect("compile");
        plan.ingest(&[
            series(
                &[("__name__", "cpu"), ("zone", "z1"), ("host", "a")],
                &[(0, 1.0), (10, 2.0)],
            ),
            series(
                &[("__name__", "cpu"), ("zone", "z1"), ("host", "b")],
                &[(0, 4.0)],
            ),
            series(
                &[("__name__", "cpu"), ("zone", "z2"), ("host", "c")],
                &[(0, 100.0), (10, 200.0)],
            ),
        ]);
        let mut got = plan.snapshot(0);
        got.sort_by(|a, b| {
            lookup(a, "zone")
                .unwrap_or("")
                .cmp(lookup(b, "zone").unwrap_or(""))
        });
        assert_eq!(got.len(), 2);
        // z1: per-series sums (1+2) + 4 = 7
        assert_eq!(got[0].samples[0].value, 7.0);
        // z2: 100+200 = 300
        assert_eq!(got[1].samples[0].value, 300.0);
    }

    #[test]
    fn ingest_first_last_over_time_pick_by_timestamp() {
        let samples = [series(
            &[("__name__", "v"), ("host", "h1")],
            &[(20, 2.0), (10, 1.0), (30, 3.0)],
        )];
        let mut p_first = compile("first_over_time(v[1m])").expect("compile");
        p_first.ingest(&samples);
        assert_eq!(p_first.snapshot(0)[0].samples[0].value, 1.0);

        let mut p_last = compile("last_over_time(v[1m])").expect("compile");
        p_last.ingest(&samples);
        assert_eq!(p_last.snapshot(0)[0].samples[0].value, 3.0);
    }

    #[test]
    fn matchers_drop_unmatched_samples() {
        let mut plan = compile(r#"sum(cpu{host="a"})"#).expect("compile");
        plan.ingest(&[
            series(&[("__name__", "cpu"), ("host", "a")], &[(0, 1.0)]),
            series(&[("__name__", "cpu"), ("host", "b")], &[(0, 99.0)]),
            series(&[("__name__", "mem"), ("host", "a")], &[(0, 50.0)]),
        ]);
        let got = plan.snapshot(0);
        assert_eq!(got[0].samples[0].value, 1.0);
    }

    #[test]
    fn reset_clears_all_accumulators() {
        let mut plan = compile("sum(cpu)").expect("compile");
        plan.ingest(&[series(&[("__name__", "cpu")], &[(0, 5.0)])]);
        assert_eq!(plan.snapshot(0).len(), 1);
        plan.reset();
        // After reset the plan has no groups → empty
        // snapshot. Re-ingest works as on a fresh plan.
        assert_eq!(plan.snapshot(0).len(), 0);
        plan.ingest(&[series(&[("__name__", "cpu")], &[(0, 7.0)])]);
        assert_eq!(plan.snapshot(0)[0].samples[0].value, 7.0);
    }

    // ---- equivalence property test (load-bearing) ----

    /// Tiny xorshift64 PRNG so the property test reproduces
    /// failures with a fixed seed and doesn't pull in a dep.
    struct XorShift64 {
        state: u64,
    }
    impl XorShift64 {
        fn new(seed: u64) -> Self {
            // xorshift requires non-zero seed.
            Self {
                state: if seed == 0 { 0xC0FFEE } else { seed },
            }
        }
        fn next_u64(&mut self) -> u64 {
            let mut x = self.state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.state = x;
            x
        }
        fn range(&mut self, lo: usize, hi_exclusive: usize) -> usize {
            lo + (self.next_u64() as usize) % (hi_exclusive - lo)
        }
        fn f64(&mut self) -> f64 {
            // Map to [0, 1). Top 53 bits → mantissa.
            let bits = self.next_u64() >> 11;
            (bits as f64) / ((1u64 << 53) as f64)
        }
    }

    /// One supported query shape, with a function that
    /// produces the batch-evaluation result so the property
    /// test can compare against streaming output.
    struct Shape {
        query: &'static str,
        // Number of distinct (host, zone) combinations to
        // generate inputs over.
        cardinality: usize,
        // Number of timestamps per series.
        samples_per_series: usize,
        // HDR-backed reducers (quantile_over_time) drop
        // negative samples. Set this for shapes that go
        // through HDR so streaming and batch both see the
        // same input.
        non_negative_values: bool,
    }

    fn supported_shapes() -> Vec<Shape> {
        vec![
            // Distributive cross-series.
            Shape {
                query: "sum(cpu)",
                cardinality: 6,
                samples_per_series: 4,
                non_negative_values: false,
            },
            Shape {
                query: "min(cpu)",
                cardinality: 5,
                samples_per_series: 3,
                non_negative_values: false,
            },
            Shape {
                query: "max(cpu)",
                cardinality: 5,
                samples_per_series: 3,
                non_negative_values: false,
            },
            Shape {
                query: "count(cpu)",
                cardinality: 5,
                samples_per_series: 3,
                non_negative_values: false,
            },
            Shape {
                query: "group(cpu)",
                cardinality: 5,
                samples_per_series: 3,
                non_negative_values: false,
            },
            Shape {
                query: "sum(cpu) by (zone)",
                cardinality: 8,
                samples_per_series: 3,
                non_negative_values: false,
            },
            Shape {
                query: "max(cpu) without (host)",
                cardinality: 8,
                samples_per_series: 3,
                non_negative_values: false,
            },
            // Algebraic cross-series.
            Shape {
                query: "avg(cpu)",
                cardinality: 6,
                samples_per_series: 4,
                non_negative_values: false,
            },
            Shape {
                query: "avg(cpu) by (zone)",
                cardinality: 8,
                samples_per_series: 3,
                non_negative_values: false,
            },
            Shape {
                query: "stddev(cpu)",
                cardinality: 6,
                samples_per_series: 4,
                non_negative_values: false,
            },
            Shape {
                query: "stdvar(cpu) by (zone)",
                cardinality: 8,
                samples_per_series: 4,
                non_negative_values: false,
            },
            // Distributive per-series rollups.
            Shape {
                query: "sum_over_time(cpu[1m])",
                cardinality: 4,
                samples_per_series: 5,
                non_negative_values: false,
            },
            Shape {
                query: "max_over_time(cpu[1m])",
                cardinality: 4,
                samples_per_series: 5,
                non_negative_values: false,
            },
            Shape {
                query: "first_over_time(cpu[1m])",
                cardinality: 4,
                samples_per_series: 5,
                non_negative_values: false,
            },
            Shape {
                query: "last_over_time(cpu[1m])",
                cardinality: 4,
                samples_per_series: 5,
                non_negative_values: false,
            },
            // Algebraic per-series rollups.
            Shape {
                query: "avg_over_time(cpu[1m])",
                cardinality: 4,
                samples_per_series: 5,
                non_negative_values: false,
            },
            Shape {
                query: "stddev_over_time(cpu[1m])",
                cardinality: 4,
                samples_per_series: 5,
                non_negative_values: false,
            },
            // Composition.
            Shape {
                query: "sum(sum_over_time(cpu[1m])) by (zone)",
                cardinality: 8,
                samples_per_series: 4,
                non_negative_values: false,
            },
            Shape {
                query: "avg(avg_over_time(cpu[1m])) by (zone)",
                cardinality: 8,
                samples_per_series: 4,
                non_negative_values: false,
            },
            // Gauge delta — both batch and streaming compute
            // `last - first` with no edge extrapolation or
            // counter-reset detection. Stays in the
            // equivalence set.
            Shape {
                query: "delta(cpu[1m])",
                cardinality: 4,
                samples_per_series: 5,
                non_negative_values: false,
            },
            // NOTE: `rate` and `increase` are deliberately
            // EXCLUDED from this equivalence test. Batch
            // applies PromQL counter-reset adjustment and
            // window-edge extrapolation; streaming sees a
            // sliding sample arrival window and uses simpler
            // `(last - first) / window_secs` semantics. The
            // divergence is by design — see the streaming
            // module doc — and is exercised by the focused
            // tests `rate_streaming_simple_window` and
            // `increase_streaming_simple_window`.
            // Scalar / vector binary ops.
            Shape {
                query: "sum(cpu) * 2",
                cardinality: 6,
                samples_per_series: 4,
                non_negative_values: false,
            },
            Shape {
                query: "100 - max(cpu)",
                cardinality: 5,
                samples_per_series: 3,
                non_negative_values: false,
            },
            Shape {
                query: "(sum(cpu) + 10) / 2",
                cardinality: 6,
                samples_per_series: 4,
                non_negative_values: false,
            },
            // Holistic (HDR-sketch). Both batch and streaming
            // route through the same HDR config — values
            // must be non-negative for HDR to record them.
            Shape {
                query: "quantile_over_time(0.5, cpu[1m])",
                cardinality: 4,
                samples_per_series: 8,
                non_negative_values: true,
            },
            Shape {
                query: "quantile_over_time(0.99, cpu[1m])",
                cardinality: 4,
                samples_per_series: 8,
                non_negative_values: true,
            },
        ]
    }

    /// Vector/vector binary shapes need TWO matching metric
    /// names in the input. Generate paired series for `a`
    /// and `b` so both sides have data, with a shared label
    /// space (host) for matching.
    fn vec_vec_supported_shapes() -> Vec<Shape> {
        vec![
            Shape {
                query: "a + b",
                cardinality: 4,
                samples_per_series: 3,
                non_negative_values: false,
            },
            Shape {
                query: "a * b",
                cardinality: 4,
                samples_per_series: 3,
                non_negative_values: false,
            },
            Shape {
                query: "a / b",
                cardinality: 4,
                samples_per_series: 3,
                non_negative_values: false,
            },
            Shape {
                query: "a + on(host) b",
                cardinality: 4,
                samples_per_series: 3,
                non_negative_values: false,
            },
            Shape {
                query: "a + ignoring(zone) b",
                cardinality: 4,
                samples_per_series: 3,
                non_negative_values: false,
            },
            // Set ops — series-level membership, not per-sample compute.
            Shape {
                query: "a and b",
                cardinality: 4,
                samples_per_series: 3,
                non_negative_values: false,
            },
            Shape {
                query: "a or b",
                cardinality: 4,
                samples_per_series: 3,
                non_negative_values: false,
            },
            Shape {
                query: "a unless b",
                cardinality: 4,
                samples_per_series: 3,
                non_negative_values: false,
            },
        ]
    }

    fn random_paired_input(rng: &mut XorShift64, shape: &Shape) -> Vec<Series> {
        let zones = ["z1", "z2"];
        let mut out = Vec::with_capacity(shape.cardinality * 2);
        for i in 0..shape.cardinality {
            let host = format!("h{i}");
            let zone = zones[i % zones.len()];
            let mut samples_a = Vec::with_capacity(shape.samples_per_series);
            let mut samples_b = Vec::with_capacity(shape.samples_per_series);
            for j in 0..shape.samples_per_series {
                let ts = (j as i64) * 100;
                let va = rng.f64() * 1000.0 - 500.0;
                let vb = rng.f64() * 1000.0 - 500.0;
                samples_a.push(Sample {
                    timestamp_ms: ts,
                    value: va,
                });
                samples_b.push(Sample {
                    timestamp_ms: ts,
                    value: vb,
                });
            }
            out.push(series(
                &[("__name__", "a"), ("host", host.as_str()), ("zone", zone)],
                &samples_a
                    .iter()
                    .map(|s| (s.timestamp_ms, s.value))
                    .collect::<Vec<_>>(),
            ));
            out.push(series(
                &[("__name__", "b"), ("host", host.as_str()), ("zone", zone)],
                &samples_b
                    .iter()
                    .map(|s| (s.timestamp_ms, s.value))
                    .collect::<Vec<_>>(),
            ));
        }
        out
    }

    /// Build a representative sample-set: `cardinality`
    /// distinct series, each with `samples_per_series`
    /// timestamps. Hosts and zones cycle across an
    /// intentionally-small alphabet so groups are populated.
    /// The optional `non_negative` flag clamps generated
    /// values to `[0, 1000)` — required for HDR-backed
    /// reducers (quantile_over_time) which drop negative
    /// samples.
    fn random_input_with(rng: &mut XorShift64, shape: &Shape, non_negative: bool) -> Vec<Series> {
        let zones = ["z1", "z2", "z3"];
        let mut out = Vec::with_capacity(shape.cardinality);
        for i in 0..shape.cardinality {
            let host = format!("h{i}");
            let zone = zones[i % zones.len()];
            let mut samples = Vec::with_capacity(shape.samples_per_series);
            for j in 0..shape.samples_per_series {
                let ts = (j as i64) * 100;
                let value = if non_negative {
                    rng.f64() * 1000.0
                } else {
                    rng.f64() * 1000.0 - 500.0
                };
                samples.push(Sample {
                    timestamp_ms: ts,
                    value,
                });
            }
            out.push(series(
                &[("__name__", "cpu"), ("host", host.as_str()), ("zone", zone)],
                &samples
                    .iter()
                    .map(|s| (s.timestamp_ms, s.value))
                    .collect::<Vec<_>>(),
            ));
        }
        out
    }

    /// One flattened (labels, sample) pair — the unit the
    /// partitioner shuffles.
    type LabeledSample = (Vec<(String, String)>, Sample);

    /// Flatten `Vec<Series>` into per-sample tuples so the
    /// partitioner can shuffle independent of series
    /// boundaries — that's what makes the property test
    /// load-bearing.
    fn flatten(series: &[Series]) -> Vec<LabeledSample> {
        let mut out = Vec::new();
        for s in series {
            for sm in &s.samples {
                out.push((s.labels.clone(), *sm));
            }
        }
        out
    }

    /// Random partition of `pairs` into `k` non-empty
    /// batches. Order within a batch is preserved — the test
    /// is about partition-then-merge associativity, not
    /// per-sample order (timestamp-aware reducers care, the
    /// rest don't).
    fn random_partition(
        rng: &mut XorShift64,
        pairs: &[LabeledSample],
        k: usize,
    ) -> Vec<Vec<LabeledSample>> {
        let mut batches: Vec<Vec<_>> = (0..k).map(|_| Vec::new()).collect();
        for p in pairs {
            let target = rng.range(0, k);
            batches[target].push(p.clone());
        }
        batches
    }

    /// Compute the batch-evaluation result for the same
    /// shape via the existing `eval::evaluate` path — that's
    /// the reference the streaming snapshot is asserted to
    /// match. Runs against an in-memory data source built
    /// from the test input.
    fn batch_result(query: &str, input: &[Series], anchor_ms: i64) -> Vec<Series> {
        struct Mem {
            series: Vec<Series>,
        }
        impl crate::eval::MetricAccess for Mem {
            fn select_range(
                &self,
                matchers: &[Matcher],
                _start: i64,
                _end: i64,
            ) -> Result<crate::eval::Vector, crate::eval::DataSourceError> {
                Ok(self
                    .series
                    .iter()
                    .filter(|s| {
                        matchers.iter().all(|m| {
                            let v = s
                                .labels
                                .iter()
                                .find(|(k, _)| k == &m.label)
                                .map(|(_, v)| v.as_str())
                                .unwrap_or("");
                            match m.op {
                                MatcherOp::Eq => v == m.value,
                                MatcherOp::Ne => v != m.value,
                                _ => v == m.value,
                            }
                        })
                    })
                    .cloned()
                    .collect())
            }
        }
        let ds = Mem {
            series: input.to_vec(),
        };
        let ctx = crate::eval::EvalContext {
            data: &ds,
            start_ms: 0,
            end_ms: anchor_ms,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let expr = parse(query).expect("parse");
        crate::eval::evaluate(&ctx, &expr).expect("evaluate")
    }

    /// Index `(labels, timestamp) → value`. Aggregate shapes
    /// emit one sample per observed timestamp; window shapes
    /// emit one sample at the query anchor. Either way the
    /// (labels, ts) tuple uniquely identifies a result point.
    fn index_by_labels_and_ts(s: &[Series]) -> BTreeMap<(Vec<(String, String)>, i64), f64> {
        let mut out = BTreeMap::new();
        for series in s {
            let mut k = series.labels.clone();
            k.sort_by(|a, b| a.0.cmp(&b.0));
            for sm in &series.samples {
                out.insert((k.clone(), sm.timestamp_ms), sm.value);
            }
        }
        out
    }

    /// `f64` equality that treats NaN==NaN and tolerates
    /// summation order drift within a small bound. Tightness
    /// matches Kahan's expected behaviour (~1 ulp per op).
    fn approx_eq(a: f64, b: f64) -> bool {
        if a.is_nan() && b.is_nan() {
            return true;
        }
        let scale = a.abs().max(b.abs()).max(1.0);
        (a - b).abs() <= scale * 1e-9
    }

    #[test]
    fn streaming_equals_batch_for_supported_shapes() {
        // Fixed seed so failures reproduce. Iteration count
        // chosen so total wall-time stays under SRD-47's
        // 50ms target.
        let mut rng = XorShift64::new(0xC0FFEEBEEF);
        const ITERATIONS_PER_SHAPE: usize = 50;
        let anchor_ms = 1000;

        let single_vec = supported_shapes();
        let vec_vec = vec_vec_supported_shapes();
        let all_shapes: Vec<(&Shape, bool)> = single_vec
            .iter()
            .map(|s| (s, false))
            .chain(vec_vec.iter().map(|s| (s, true)))
            .collect();
        for (shape, is_vec_vec) in all_shapes {
            for _trial in 0..ITERATIONS_PER_SHAPE {
                let input = if is_vec_vec {
                    random_paired_input(&mut rng, shape)
                } else {
                    random_input_with(&mut rng, shape, shape.non_negative_values)
                };
                let batch = batch_result(shape.query, &input, anchor_ms);
                let batch_idx = index_by_labels_and_ts(&batch);

                // Pick a partition count [1, 6]. k=1 is
                // batch-equivalent (sanity); higher values
                // exercise the merge path.
                let k = rng.range(1, 7);
                let pairs = flatten(&input);
                let batches = random_partition(&mut rng, &pairs, k);

                let mut plan =
                    compile_streaming(&parse(shape.query).expect("parse")).expect("compile");
                for batch_pairs in &batches {
                    for (labels, sample) in batch_pairs {
                        plan.ingest_sample(labels, sample);
                    }
                }
                let stream = plan.snapshot(anchor_ms);
                let stream_idx = index_by_labels_and_ts(&stream);

                if batch_idx.len() != stream_idx.len() {
                    panic!(
                        "shape {:?}: result cardinality differs — batch={} stream={}\n  batch keys: {:?}\n  stream keys: {:?}",
                        shape.query,
                        batch_idx.len(),
                        stream_idx.len(),
                        batch_idx.keys().collect::<Vec<_>>(),
                        stream_idx.keys().collect::<Vec<_>>(),
                    );
                }
                for (k, batch_v) in &batch_idx {
                    let stream_v = stream_idx.get(k).copied().unwrap_or(f64::NAN);
                    if !approx_eq(*batch_v, stream_v) {
                        panic!(
                            "shape {:?}: divergence at {:?} — batch={} stream={}",
                            shape.query, k, batch_v, stream_v,
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn kahan_summation_resists_order_drift() {
        // Classic Kahan stress: many small values plus one
        // large value. Naive summation loses the tail.
        let mut left = KahanAcc::default();
        let r = SumReducer;
        r.ingest(&mut left, &s(0, 1e16));
        for i in 0..1000 {
            r.ingest(&mut left, &s(i + 1, 1.0));
        }
        let mut right = KahanAcc::default();
        for i in 0..1000 {
            r.ingest(&mut right, &s(i + 1, 1.0));
        }
        r.ingest(&mut right, &s(0, 1e16));
        let lv = r.snapshot(&left);
        let rv = r.snapshot(&right);
        // Kahan keeps both within ~1 ulp; naive f64 would
        // diverge by ~1000.
        assert!(
            (lv - rv).abs() < 2.0,
            "kahan order drift too large: {lv} vs {rv}"
        );
        // And both are within 1 of the true total `1e16 + 1000`.
        let truth = 1e16 + 1000.0;
        assert!((lv - truth).abs() <= 1.0);
    }
}
