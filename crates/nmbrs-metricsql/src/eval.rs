// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! MetricsQL evaluator. AST → query plan → result, against a
//! pluggable [`DataSource`].
//!
//! This is the first cut. The shape will fill out incremen-
//! tally as evaluator paths land:
//!
//!   1. Selector evaluation → `DataSource::fetch`
//!   2. Range vectors (`[5m]` rollups, offset, `@`)
//!   3. Aggregations (`sum/avg/min/max/count` by/without)
//!   4. Binary ops with vector matching
//!   5. Range queries (stepped multi-instant evaluation)
//!   6. Rollup-consumer functions (`rate`, `*_over_time`)
//!   7. [`DataSource`] boundary formalised
//!   8. Set ops + filter-mode comparisons
//!   9. Vector-matching modifiers (`on`/`ignoring`/
//!      `group_left`/`group_right`)                         ← *here*
//!
//! The boundary review at step (7) was informed by the four
//! call shapes the previous passes exposed against `fetch`:
//! instant `(T, T)`, windowed `(T-w, T)`, range-query loop of
//! either, and rollup-function-over-window. All four route
//! through a single `(matchers, start, end)` signature, and
//! none of them has yet surfaced a need for streaming or
//! batched-prefetch — so the trait stays one method wide.
//! The notable shape change at this step is admitting
//! backend failures through a typed [`DataSourceError`]
//! instead of pretending storage can't fail.
//!
//! After (2)–(3) light up, the [`DataSource`] trait gets a
//! second pass to harden the boundary against real call-site
//! pressure (range queries, label-set materialization,
//! streaming vs. materialized aggregate inputs). Until then
//! the shape here is provisional — see
//! `project_metricsql_eval_boundary.md`.

use crate::ast::{
    AggrModifier, AggrModifierOp, BinaryOp, BinaryOpExpr, DurationExpr, Expr, FuncExpr,
    GroupModifier, GroupOp, JoinModifier, JoinOp, LabelFilter, LabelFilterOp, MetricExpr,
    NumberExpr, RollupExpr,
};

/// Query-result and selector shapes are the **metrics access
/// library's** native types (SRD-86 §"The metric-reader surface").
/// `nmbrs-metrics` is the foundational data-access library; the
/// MetricsQL engine evaluates *over* its `Vector` shape and owns no
/// result or selector types of its own. Re-exported here so the
/// engine's many `Series`/`Sample`/`Matcher` references read the
/// shared types unchanged.
pub use nmbrs_metrics::queryapi::{
    MatchOp as MatcherOp, Matcher, QueryError as DataSourceError, Sample, Series, Vector,
};

/// The engine's data backend is the metrics access *service*
/// ([`MetricAccess`]) — `nmbrs-metrics` owns the contract. The engine
/// locates a service at runtime (live or sqlite) and reads `Vector`s
/// through `select_range` / `select_instant`. Re-exported here for the
/// engine's `&dyn MetricAccess` references and the crate's public API.
pub use nmbrs_metrics::queryapi::MetricAccess;

/// Evaluation context: the data source plus the time range
/// the query operates over. Step size matters for range
/// queries; instant queries use `start_ms == end_ms`.
///
/// **Instant-vector lookback delta** (`lookback_ms`): for
/// instant queries (`start_ms == end_ms`) the selector path
/// fetches `[T - lookback, T]` and keeps the latest sample
/// per series — matching PromQL's stale-tolerance window
/// for instant evaluation. The default is `None`, which
/// preserves the strict `[T, T]` fetch (a sample must land
/// exactly at `T` to be returned). Real consumers (the
/// `nmbrs metrics query` CLI) typically set this to 5
/// minutes or so to absorb cadence skew. Inside a rollup
/// (`metric[5m]`) the inner ctx has `start_ms != end_ms`,
/// so lookback never applies — the rollup's own window IS
/// the lookback.
pub struct EvalContext<'a> {
    pub data: &'a dyn MetricAccess,
    pub start_ms: i64,
    pub end_ms: i64,
    pub step_ms: i64,
    /// `None` → strict instant semantics (caller wanted
    /// exactly that timestamp). `Some(n)` → for instant
    /// queries, look back `n` milliseconds for the latest
    /// sample per series.
    pub lookback_ms: Option<i64>,
    /// Outer query bounds, used by `@ start()` / `@ end()`.
    /// `None` → defaults to `start_ms` / `end_ms`. Set
    /// explicitly by [`evaluate_range`] so per-step inner
    /// contexts can still resolve the outer query window.
    pub query_start_ms: Option<i64>,
    pub query_end_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub enum EvalError {
    NotYetImplemented(&'static str),
    /// A node carries shape that the evaluator can't honour
    /// (e.g. a template-ref filter that should have been
    /// expanded by `parse`). Indicates a missed canonicalisation
    /// step rather than a user-facing parse error.
    InvalidShape(String),
    /// User-facing input the evaluator couldn't interpret —
    /// malformed duration text, unsupported `@` expression,
    /// etc.
    BadValue(String),
    /// Failure raised by the [`DataSource`] backend. Wraps
    /// the underlying message verbatim so callers can surface
    /// it.
    DataSource(DataSourceError),
}

impl From<DataSourceError> for EvalError {
    fn from(e: DataSourceError) -> Self {
        EvalError::DataSource(e)
    }
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::NotYetImplemented(what) => {
                write!(f, "evaluator: {what} is not yet implemented")
            }
            EvalError::InvalidShape(msg) => {
                write!(f, "evaluator: invalid AST shape: {msg}")
            }
            EvalError::BadValue(msg) => {
                write!(f, "evaluator: {msg}")
            }
            EvalError::DataSource(e) => {
                write!(f, "evaluator: {e}")
            }
        }
    }
}

impl std::error::Error for EvalError {}

/// Evaluate `expr` as a **range query**: re-run it at every
/// step from `ctx.start_ms` to `ctx.end_ms` (inclusive), and
/// merge each step's output series by label identity. Each
/// per-step evaluation observes a degenerate `[T, T]` instant
/// context with the original step retained for `i`-relative
/// durations.
///
/// `step_ms` must be `> 0`; instant queries
/// (`start_ms == end_ms`) work too — they produce a single
/// step at `start_ms`.
///
/// The naïve loop fetches the underlying data once per step
/// even when the same window would suffice for many steps in
/// a row. That's the first real call-site shape pushing on
/// the storage trait — the optimisation lands when the
/// boundary is formalised (see
/// `project_metricsql_eval_boundary.md`); behaviour stays
/// correct in the meantime.
pub fn evaluate_range(ctx: &EvalContext<'_>, expr: &Expr) -> Result<Vec<Series>, EvalError> {
    if ctx.step_ms <= 0 {
        return Err(EvalError::BadValue(format!(
            "evaluate_range requires step_ms > 0, got {}",
            ctx.step_ms
        )));
    }
    if ctx.end_ms < ctx.start_ms {
        return Err(EvalError::BadValue(format!(
            "evaluate_range requires end_ms >= start_ms, got start={} end={}",
            ctx.start_ms, ctx.end_ms
        )));
    }
    let mut merged: Vec<Series> = Vec::new();
    let mut t = ctx.start_ms;
    let query_start = ctx.query_start_ms.unwrap_or(ctx.start_ms);
    let query_end = ctx.query_end_ms.unwrap_or(ctx.end_ms);
    loop {
        let step_ctx = EvalContext {
            data: ctx.data,
            start_ms: t,
            end_ms: t,
            step_ms: ctx.step_ms,
            // Propagate lookback so each step's instant
            // evaluation honours the same stale-tolerance.
            lookback_ms: ctx.lookback_ms,
            // Pin the outer range so `@ start()` / `@ end()`
            // resolve to the original query bounds, not the
            // per-step instant.
            query_start_ms: Some(query_start),
            query_end_ms: Some(query_end),
        };
        let step_result = evaluate(&step_ctx, expr)?;
        merge_step_into(&mut merged, step_result);
        if t == ctx.end_ms {
            break;
        }
        t = (t.saturating_add(ctx.step_ms)).min(ctx.end_ms);
    }
    Ok(merged)
}

/// Append every series in `step` to `merged`, joining samples
/// onto an existing entry when its label set already appears.
/// Preserves first-seen order.
fn merge_step_into(merged: &mut Vec<Series>, step: Vec<Series>) {
    for s in step {
        match merged
            .iter_mut()
            .find(|m| label_sets_equal(&m.labels, &s.labels))
        {
            Some(existing) => existing.samples.extend(s.samples),
            None => merged.push(s),
        }
    }
}

/// Evaluate a parsed MetricsQL expression against the
/// context's data source. Currently dispatches the subset
/// covered by the first evaluator path; everything else
/// returns [`EvalError::NotYetImplemented`].
pub fn evaluate(ctx: &EvalContext<'_>, expr: &Expr) -> Result<Vec<Series>, EvalError> {
    match expr {
        Expr::Metric(me) => evaluate_metric_expr(ctx, me),
        Expr::Number(n) => Ok(scalar_series(ctx, n.value)),
        Expr::String(_) => Err(EvalError::NotYetImplemented("string literals")),
        Expr::Duration(_) => Err(EvalError::NotYetImplemented("duration literals")),
        Expr::Func(f) => evaluate_func(ctx, f),
        Expr::Binary(b) => evaluate_binary(ctx, b),
        Expr::Rollup(re) => evaluate_rollup(ctx, re),
        Expr::Paren(_) => Err(EvalError::NotYetImplemented("parens groups")),
        Expr::With(_) => Err(EvalError::InvalidShape(
            "WithExpr survived into evaluation — caller should use `parse` (which expands) not `parse_for_prettify`".into())),
    }
}

/// Lift a numeric literal into the series shape the rest of
/// the evaluator speaks. Mirrors upstream's "scalar is a
/// single label-less series" convention. For instant queries
/// the sample lands at `end_ms`; range queries (when they
/// arrive) will populate one sample per step.
fn scalar_series(ctx: &EvalContext<'_>, value: f64) -> Vec<Series> {
    vec![Series {
        labels: Vec::new(),
        samples: vec![Sample {
            timestamp_ms: ctx.end_ms,
            value,
        }],
    }]
}

/// Evaluate a single instant-vector selector. Each `or` group
/// in `label_filterss` becomes one independent fetch; the
/// union of returned series (de-duplicated by label set) is
/// the result.
///
/// Label filters with `is_template_ref` or non-empty
/// `value_expr` indicate the AST wasn't fully canonicalised
/// and surface as [`EvalError::InvalidShape`] — `parse` should
/// always resolve them before reaching here.
fn evaluate_metric_expr(ctx: &EvalContext<'_>, me: &MetricExpr) -> Result<Vec<Series>, EvalError> {
    if me.label_filterss.is_empty() {
        return Ok(Vec::new());
    }
    // Instant-vector lookback delta: when the query is
    // instant (`start_ms == end_ms`) AND the caller asked
    // for a lookback window, fetch `[T - lookback, T]` and
    // collapse each series down to its latest sample. This
    // matches PromQL's stale-tolerance semantics and absorbs
    // the cadence-skew issue that real `metrics.db` data
    // exposed (counters and summaries land at slightly
    // different timestamps within the same cycle).
    let (fetch_start, fetch_end, instant_lookback) =
        match (ctx.lookback_ms, ctx.start_ms == ctx.end_ms) {
            (Some(lb), true) if lb > 0 => (ctx.end_ms - lb, ctx.end_ms, true),
            _ => (ctx.start_ms, ctx.end_ms, false),
        };
    let mut out: Vec<Series> = Vec::new();
    let mut seen: Vec<Vec<(String, String)>> = Vec::new();
    for group in &me.label_filterss {
        let matchers = filters_to_matchers(group)?;
        let fetched = ctx
            .data
            .select_range(&matchers, fetch_start, fetch_end)?
            .into_series();
        for s in fetched {
            if seen.iter().any(|prev| label_sets_equal(prev, &s.labels)) {
                continue;
            }
            seen.push(s.labels.clone());
            if instant_lookback {
                // Pick the latest sample in the lookback
                // window; project it to the query anchor.
                // Drops series with no samples in window.
                let Some(latest) = s.samples.iter().max_by_key(|sm| sm.timestamp_ms) else {
                    continue;
                };
                out.push(Series {
                    labels: s.labels,
                    samples: vec![Sample {
                        timestamp_ms: ctx.end_ms,
                        value: latest.value,
                    }],
                });
            } else if !s.samples.is_empty() {
                // Strict instant / range mode: drop series
                // that have no samples in the requested
                // window. Matches PromQL's "no data → no
                // series" intuition; downstream aggregates
                // and binary ops produce cleaner output.
                out.push(s);
            }
        }
    }
    Ok(out)
}

/// Evaluate a [`BinaryOpExpr`]. Three shapes:
///   - **scalar/scalar** — both sides label-less, one sample
///     each → one synthetic series with the op applied.
///   - **scalar/vector** (or vice versa) — broadcast the
///     scalar value across every sample of the vector.
///   - **vector/vector** — match left series to right series
///     by label set (excluding `__name__`) and apply the op
///     per timestamp.
///
/// Comparison operators with the `bool` modifier produce 0/1
/// per upstream; without `bool` they act as filters that drop
/// samples failing the predicate.
///
/// Vector-matching modifiers (`on`/`ignoring`/`group_left`/
/// `group_right`) and the set ops (`and`/`or`/`unless`) are
/// honoured — see [`combine_vectors_modified`] and
/// [`combine_set_op`].
fn evaluate_binary(ctx: &EvalContext<'_>, b: &BinaryOpExpr) -> Result<Vec<Series>, EvalError> {
    // Scalar-ness is an AST property, not a result-shape
    // property. `sum(cpu)` produces a single series with no
    // labels but is still a vector — it must broadcast over
    // its own per-timestamp samples, not collapse to a single
    // end_ms sample. Inspecting the AST keeps the
    // distinction crisp.
    let left_is_scalar_expr = is_scalar_expr(&b.left);
    let right_is_scalar_expr = is_scalar_expr(&b.right);
    let left = evaluate(ctx, &b.left)?;
    let right = evaluate(ctx, &b.right)?;
    // Set ops dispatch separately — they reshape series and
    // sample lists rather than computing per-pair values.
    // The matching modifier (`on`/`ignoring`) still steers
    // their key computation, but join modifiers (group_left/
    // group_right) aren't legal there.
    if matches!(b.op, BinaryOp::And | BinaryOp::Or | BinaryOp::Unless) {
        if b.join_modifier.is_some() {
            return Err(EvalError::BadValue(format!(
                "{:?} doesn't support group_left / group_right",
                b.op
            )));
        }
        return Ok(combine_set_op(
            b.op,
            &left,
            &right,
            b.group_modifier.as_ref(),
        ));
    }
    // Filter-mode comparisons use the same per-sample compute
    // as `bool`-mode but drop the sample when the predicate is
    // false (NaN signals "drop"). Series that end up empty
    // are pruned from the output.
    let bool_mod = b.bool_modifier;
    let combine_value = move |l: f64, r: f64| eval_binary_value(b.op, l, r, bool_mod);
    let drop_nan_results = is_cmp_op(b.op) && !bool_mod;

    // Combine the AST signal with the result shape. A
    // scalar-shaped result from a vector expression (e.g.
    // `sum(cpu)` with no `by(...)`) is still a vector.
    let left_is_scalar = left_is_scalar_expr && is_scalar_series(&left);
    let right_is_scalar = right_is_scalar_expr && is_scalar_series(&right);

    let result = if left_is_scalar && right_is_scalar {
        let l = left[0].samples.first().map(|s| s.value).unwrap_or(f64::NAN);
        let r = right[0]
            .samples
            .first()
            .map(|s| s.value)
            .unwrap_or(f64::NAN);
        scalar_series(ctx, combine_value(l, r))
    } else if left_is_scalar {
        let l = left[0].samples.first().map(|s| s.value).unwrap_or(f64::NAN);
        broadcast_scalar(&right, l, true, &combine_value)
    } else if right_is_scalar {
        let r = right[0]
            .samples
            .first()
            .map(|s| s.value)
            .unwrap_or(f64::NAN);
        broadcast_scalar(&left, r, false, &combine_value)
    } else {
        combine_vectors_modified(
            &left,
            &right,
            b.group_modifier.as_ref(),
            b.join_modifier.as_ref(),
            &combine_value,
        )
    };

    if drop_nan_results {
        Ok(prune_nan_samples(result))
    } else {
        Ok(result)
    }
}

/// Strip NaN-valued samples and drop series that end up
/// empty. Used by filter-mode comparisons (`cpu > 3` without
/// `bool`): the per-sample compute returns NaN for "predicate
/// false", which we want to elide rather than carry forward.
fn prune_nan_samples(input: Vec<Series>) -> Vec<Series> {
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

/// Set ops (`and` / `or` / `unless`). Pair series by the
/// matching key (labels-without-name by default; `on`/
/// `ignoring` reshape it), then filter per-sample by
/// timestamp.
///
/// - `vec1 and vec2` — keep `vec1`'s sample when `vec2` has a
///   sample at the same (key, timestamp).
/// - `vec1 or vec2` — every `vec1` sample, plus every `vec2`
///   sample whose (key, timestamp) is absent from `vec1`.
/// - `vec1 unless vec2` — `vec1`'s samples whose (key, timestamp)
///   does NOT appear in `vec2`.
///
/// Result series preserve `vec1`'s full labels (`and` /
/// `unless` / left half of `or`) or `vec2`'s (right half of
/// `or`); upstream keeps the operand's own metric name.
pub(crate) fn combine_set_op(
    op: BinaryOp,
    left: &[Series],
    right: &[Series],
    modifier: Option<&GroupModifier>,
) -> Vec<Series> {
    let right_index = build_sample_index(right, modifier);
    let left_index = build_sample_index(left, modifier);
    let mut out: Vec<Series> = Vec::new();
    for ls in left {
        let key = match_key(&ls.labels, modifier);
        let kept: Vec<Sample> = ls
            .samples
            .iter()
            .filter(|sm| {
                let present_on_right = right_index
                    .iter()
                    .any(|(k, ts)| label_sets_equal(k, &key) && *ts == sm.timestamp_ms);
                match op {
                    BinaryOp::And => present_on_right,
                    BinaryOp::Unless => !present_on_right,
                    BinaryOp::Or => true,
                    _ => unreachable!(),
                }
            })
            .cloned()
            .collect();
        if !kept.is_empty() {
            out.push(Series {
                labels: ls.labels.clone(),
                samples: kept,
            });
        }
    }
    if matches!(op, BinaryOp::Or) {
        for rs in right {
            let key = match_key(&rs.labels, modifier);
            let kept: Vec<Sample> = rs
                .samples
                .iter()
                .filter(|sm| {
                    let present_on_left = left_index
                        .iter()
                        .any(|(k, ts)| label_sets_equal(k, &key) && *ts == sm.timestamp_ms);
                    !present_on_left
                })
                .cloned()
                .collect();
            if !kept.is_empty() {
                out.push(Series {
                    labels: rs.labels.clone(),
                    samples: kept,
                });
            }
        }
    }
    out
}

/// Build a `(matching-key, timestamp_ms)` membership index
/// for a series list. Linear scan suffices for the
/// instant-query and small-range workloads the evaluator
/// faces today; if a profiler ever flags it we can swap in a
/// hash structure keyed on canonical labels.
fn build_sample_index(
    input: &[Series],
    modifier: Option<&GroupModifier>,
) -> Vec<(Vec<(String, String)>, i64)> {
    let mut out = Vec::new();
    for s in input {
        let key = match_key(&s.labels, modifier);
        for sm in &s.samples {
            out.push((key.clone(), sm.timestamp_ms));
        }
    }
    out
}

pub(crate) fn is_cmp_op(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
    )
}

/// True when a series list looks like a constant — exactly
/// one entry with no labels. Number literals produce this
/// shape directly; intermediate aggregate results keep their
/// `by`/`without` labels and don't qualify.
fn is_scalar_series(s: &[Series]) -> bool {
    s.len() == 1 && s[0].labels.is_empty()
}

/// True when an expression IS scalar by construction — a
/// numeric literal or another scalar-producing form. Used to
/// avoid mis-classifying unlabeled aggregate results as
/// scalars; an `is_scalar_series` check that accepts
/// `sum(cpu)` would collapse its per-timestamp samples down
/// to one and break range-query equivalence.
fn is_scalar_expr(e: &Expr) -> bool {
    match e {
        Expr::Number(_) | Expr::Duration(_) => true,
        // Parens around a scalar passes through. After
        // `remove_parens_expr` runs at parse time these are
        // rare, but keep the recursion for completeness.
        Expr::Paren(p) => p.exprs.len() == 1 && is_scalar_expr(&p.exprs[0]),
        _ => false,
    }
}

/// Per-sample binary op. Comparison ops with `bool_mod`
/// return 0/1; arithmetic ops use the same machinery the
/// parser's constant-folder uses, so result semantics agree
/// (NaN propagation, division-by-zero → ±inf, etc.).
#[cfg(test)]
mod align_tests {
    use super::*;

    fn s(ts: i64, v: f64) -> Sample {
        Sample {
            timestamp_ms: ts,
            value: v,
        }
    }

    #[test]
    fn instant_vectors_combine_despite_different_write_times() {
        // The regression: two metric families written on different cadence ticks
        // each resolved alone but their ratio came back empty, because matching
        // required identical timestamps. At an instant the timestamp records when
        // the datum was written, not when it is evaluated.
        let left = vec![s(1_000, 100.0)];
        let right = vec![s(1_500, 4.0)];
        let out = align_and_combine(&left, &right, &|a, b| a / b);
        assert_eq!(out.len(), 1, "instant operands must combine");
        assert_eq!(out[0].value, 25.0);
        assert_eq!(
            out[0].timestamp_ms, 1_500,
            "result carries the freshest input"
        );
    }

    #[test]
    fn range_vectors_still_align_strictly() {
        // With real series on both sides, combining across instants would invent
        // a correlation the data does not contain, so equality still governs.
        let left = vec![s(1_000, 10.0), s(2_000, 20.0), s(3_000, 30.0)];
        let right = vec![s(2_000, 2.0), s(3_000, 3.0), s(4_000, 4.0)];
        let out = align_and_combine(&left, &right, &|a, b| a / b);
        assert_eq!(out.len(), 2, "only the shared instants combine");
        assert_eq!(out[0].timestamp_ms, 2_000);
        assert_eq!(out[0].value, 10.0);
        assert_eq!(out[1].timestamp_ms, 3_000);
        assert_eq!(out[1].value, 10.0);
    }

    #[test]
    fn single_sample_broadcasts_across_a_series() {
        // A metric written once per phase divided by one sampled per tick is an
        // ordinary query; strict alignment used to return nothing for it.
        let once = vec![s(5_000, 100.0)];
        let ticks = vec![s(1_000, 2.0), s(2_000, 4.0), s(3_000, 5.0)];
        let out = align_and_combine(&once, &ticks, &|a, b| a / b);
        assert_eq!(out.len(), 3, "the single value applies throughout");
        assert_eq!(out[0].value, 50.0);
        assert_eq!(out[1].value, 25.0);
        assert_eq!(out[2].value, 20.0);
        // Timestamps come from the series side, which is the one that varies.
        assert_eq!(out[2].timestamp_ms, 3_000);
        // Symmetric in the other direction, with the operand order preserved.
        let out = align_and_combine(&ticks, &once, &|a, b| a / b);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].value, 2.0 / 100.0);
    }

    #[test]
    fn empty_operand_yields_nothing() {
        assert!(align_and_combine(&[], &[s(1, 1.0)], &|a, b| a + b).is_empty());
        assert!(align_and_combine(&[s(1, 1.0)], &[], &|a, b| a + b).is_empty());
    }
}

pub(crate) fn eval_binary_value(op: BinaryOp, l: f64, r: f64, bool_mod: bool) -> f64 {
    use BinaryOp::*;
    match op {
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
            let cmp = match op {
                Eq => bin_eq(l, r),
                Ne => bin_neq(l, r),
                Gt => l > r,
                Lt => l < r,
                Ge => l >= r,
                Le => l <= r,
                _ => unreachable!(),
            };
            if bool_mod {
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
        // `and` / `or` / `unless` are dispatched as set ops
        // before reaching `eval_binary_value`; guard with
        // NaN so the match stays total.
        And | Or | Unless => f64::NAN,
    }
}

fn bin_eq(l: f64, r: f64) -> bool {
    if l.is_nan() {
        return r.is_nan();
    }
    l == r
}

fn bin_neq(l: f64, r: f64) -> bool {
    if l.is_nan() {
        return !r.is_nan();
    }
    if r.is_nan() {
        return true;
    }
    l != r
}

/// Apply a scalar value to every sample of every series in
/// `vector`. `scalar_on_left` controls operand order so non-
/// commutative ops (`-`, `/`, `^`, `%`, `atan2`) match the
/// source `<scalar> <op> <vector>` vs. `<vector> <op> <scalar>`
/// orientation.
fn broadcast_scalar(
    vector: &[Series],
    scalar: f64,
    scalar_on_left: bool,
    combine: &impl Fn(f64, f64) -> f64,
) -> Vec<Series> {
    vector
        .iter()
        .map(|s| {
            let labels = labels_after_op(&s.labels);
            let samples = s
                .samples
                .iter()
                .map(|sm| {
                    let value = if scalar_on_left {
                        combine(scalar, sm.value)
                    } else {
                        combine(sm.value, scalar)
                    };
                    Sample {
                        timestamp_ms: sm.timestamp_ms,
                        value,
                    }
                })
                .collect();
            Series { labels, samples }
        })
        .collect()
}

/// Vector-vector match with optional modifiers. Three shapes:
///
/// - **1:1** (no `join_modifier`): pair each left series
///   with the right series sharing the same matching key.
///   Result labels = the matching key.
///
/// - **Many:1** (`group_left(extras)`): every left series
///   pairs with the right series sharing the matching key;
///   result keeps left's labels (minus `__name__`) and
///   copies the listed `extras` over from the matched right.
///
/// - **1:Many** (`group_right(extras)`): symmetric of the
///   above; the right side becomes "many", result keeps
///   right's labels plus `extras` copied from the matched
///   left.
///
/// `group_modifier` (`on(...)` / `ignoring(...)`) only
/// changes how the matching key is computed; it doesn't
/// affect the cardinality.
pub(crate) fn combine_vectors_modified(
    left: &[Series],
    right: &[Series],
    group_modifier: Option<&GroupModifier>,
    join_modifier: Option<&JoinModifier>,
    combine: &impl Fn(f64, f64) -> f64,
) -> Vec<Series> {
    let mut out: Vec<Series> = Vec::new();
    let many_left = matches!(join_modifier, Some(j) if matches!(j.op, JoinOp::GroupLeft));
    let many_right = matches!(join_modifier, Some(j) if matches!(j.op, JoinOp::GroupRight));

    if !many_left && !many_right {
        // Default 1:1.
        for ls in left {
            let key = match_key(&ls.labels, group_modifier);
            let Some(rs) = right
                .iter()
                .find(|rs| label_sets_equal(&key, &match_key(&rs.labels, group_modifier)))
            else {
                continue;
            };
            let samples = align_and_combine(&ls.samples, &rs.samples, combine);
            out.push(Series {
                labels: result_labels_one_to_one(&ls.labels, group_modifier),
                samples,
            });
        }
        return out;
    }

    // Many:1 / 1:Many. The "many" side is iterated as the
    // primary; extras from the join modifier are copied from
    // the "one" side onto each result series.
    let extras: &[String] = join_modifier.map(|j| j.labels.as_slice()).unwrap_or(&[]);
    let (primary, secondary) = if many_left {
        (left, right)
    } else {
        (right, left)
    };
    for ps in primary {
        let key = match_key(&ps.labels, group_modifier);
        let Some(ss) = secondary
            .iter()
            .find(|ss| label_sets_equal(&key, &match_key(&ss.labels, group_modifier)))
        else {
            continue;
        };
        // Sample alignment: orient operands so `combine`
        // sees them in source order (left, right) regardless
        // of which side is "primary".
        let samples = if many_left {
            align_and_combine(&ps.samples, &ss.samples, combine)
        } else {
            align_and_combine(&ss.samples, &ps.samples, combine)
        };
        let mut labels = labels_drop_name(&ps.labels);
        copy_labels_into(&mut labels, &ss.labels, extras);
        out.push(Series { labels, samples });
    }
    out
}

/// Compute the matching key for a series's label set under
/// the active group modifier. The default (no modifier) is
/// "every label except `__name__`".
pub(crate) fn match_key(
    labels: &[(String, String)],
    modifier: Option<&GroupModifier>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = match modifier {
        None => labels
            .iter()
            .filter(|(k, _)| k != "__name__")
            .cloned()
            .collect(),
        Some(GroupModifier {
            op: GroupOp::On,
            labels: keep,
        }) => labels
            .iter()
            .filter(|(k, _)| keep.iter().any(|w| w == k))
            .cloned()
            .collect(),
        Some(GroupModifier {
            op: GroupOp::Ignoring,
            labels: drop,
        }) => labels
            .iter()
            .filter(|(k, _)| k != "__name__" && !drop.iter().any(|w| w == k))
            .cloned()
            .collect(),
    };
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Result labels for default 1:1 matching: the matching key
/// itself. With no modifier that's the lhs label set minus
/// `__name__`; with `on(...)` it's exactly the listed
/// labels; with `ignoring(...)` it's lhs minus `__name__`
/// minus the ignored set. Equivalent to `match_key` here —
/// the function exists so the call site reads its intent.
pub(crate) fn result_labels_one_to_one(
    labels: &[(String, String)],
    modifier: Option<&GroupModifier>,
) -> Vec<(String, String)> {
    match_key(labels, modifier)
}

pub(crate) fn labels_drop_name(labels: &[(String, String)]) -> Vec<(String, String)> {
    labels
        .iter()
        .filter(|(k, _)| k != "__name__")
        .cloned()
        .collect()
}

/// Copy the `extras` labels from `src` onto `dst`. Used by
/// `group_left(extras)` / `group_right(extras)` to enrich the
/// "many"-side result with carry-over labels from the "one"
/// side. Pre-existing entries on `dst` are overwritten —
/// matches upstream semantics where the join modifier wins
/// over the primary's own label.
pub(crate) fn copy_labels_into(
    dst: &mut Vec<(String, String)>,
    src: &[(String, String)],
    extras: &[String],
) {
    for label in extras {
        let Some((_, v)) = src.iter().find(|(k, _)| k == label) else {
            continue;
        };
        match dst.iter_mut().find(|(k, _)| k == label) {
            Some(entry) => entry.1 = v.clone(),
            None => dst.push((label.clone(), v.clone())),
        }
    }
}

/// Result-series labels: drop `__name__` (per upstream:
/// arithmetic between vectors loses metric identity).
fn labels_after_op(labels: &[(String, String)]) -> Vec<(String, String)> {
    labels
        .iter()
        .filter(|(k, _)| k != "__name__")
        .cloned()
        .collect()
}

/// Inner-join two sample streams on timestamp and apply
/// `combine` to each matched pair. Timestamps unique to one
/// side don't contribute — there's no value to operate
/// against.
pub(crate) fn align_and_combine(
    left: &[Sample],
    right: &[Sample],
    combine: &impl Fn(f64, f64) -> f64,
) -> Vec<Sample> {
    // INSTANT-VECTOR CASE: one sample per side. Both operands are the value of
    // their series *at the evaluation instant*, so they combine unconditionally —
    // the sample's own timestamp records when the datum was last written, not
    // when it is being evaluated, and two metric families are almost never
    // written on the same tick.
    //
    // Requiring exact timestamp equality here silently produced EMPTY results
    // for every cross-family expression whose operands came from different
    // cadence subscriptions: each side resolved fine alone, and their ratio
    // vanished. Result carries the later timestamp, so downstream sees the
    // freshest input the value depends on.
    if left.len() == 1 && right.len() == 1 {
        let (l, r) = (&left[0], &right[0]);
        return vec![Sample {
            timestamp_ms: l.timestamp_ms.max(r.timestamp_ms),
            value: combine(l.value, r.value),
        }];
    }
    // MIXED CASE: one side is a single instant value, the other a series. The
    // reach of this case is wider than it first appears: it is also what makes a
    // TIMER-STAT operand work. A timer stat for a finished phase has one
    // resurrected last value while its partner has many, so before this branch
    // existed such divisions came back empty and looked like a separate
    // datasource-selection defect. There is only one bug here.
    //
    // single value is the operand's value throughout, so broadcast it rather
    // than demanding a timestamp twin it cannot have — a metric written once per
    // phase divided by one sampled per tick is an ordinary, meaningful query,
    // and strict alignment silently returned nothing for it.
    if left.len() == 1 && right.len() > 1 {
        let l = &left[0];
        return right
            .iter()
            .map(|r| Sample {
                timestamp_ms: r.timestamp_ms,
                value: combine(l.value, r.value),
            })
            .collect();
    }
    if right.len() == 1 && left.len() > 1 {
        let r = &right[0];
        return left
            .iter()
            .map(|l| Sample {
                timestamp_ms: l.timestamp_ms,
                value: combine(l.value, r.value),
            })
            .collect();
    }
    // RANGE case: genuine time series on both sides. Align strictly by
    // timestamp — combining samples from different instants would invent a
    // correlation that the data does not contain.
    let mut out: Vec<Sample> = Vec::new();
    for l in left {
        if let Some(r) = right.iter().find(|r| r.timestamp_ms == l.timestamp_ms) {
            out.push(Sample {
                timestamp_ms: l.timestamp_ms,
                value: combine(l.value, r.value),
            });
        }
    }
    out
}

/// Element-wise transforms: same series, same timestamps, one value mapped.
///
/// Distinct from an aggregate (which collapses series) and a rollup (which
/// collapses a range window). The evaluator had neither surface, so a query
/// using one failed with "non-aggregate / non-rollup function calls is not yet
/// implemented" even though the grammar accepted it.
///
/// `round` is the one with teeth here. The metrics store coalesces a gauge as a
/// weighted MEAN over its cadence interval, which is right for a level but
/// wrong for a COUNT: a count set partway through a window reads 0.9994 instead
/// of 1, forever. `round()` at query time is how a count-valued gauge is read
/// back as the integer it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransformFn {
    /// `round(q[, nearest])` — nearest multiple of `nearest` (default 1).
    Round,
    Abs,
    Ceil,
    Floor,
}

impl TransformFn {
    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "round" => Some(Self::Round),
            "abs" => Some(Self::Abs),
            "ceil" => Some(Self::Ceil),
            "floor" => Some(Self::Floor),
            _ => None,
        }
    }
}

fn evaluate_transform(
    ctx: &EvalContext<'_>,
    f: &FuncExpr,
    t: TransformFn,
) -> Result<Vec<Series>, EvalError> {
    let Some(arg) = f.args.first() else {
        return Err(EvalError::NotYetImplemented(
            "transform function called with no argument",
        ));
    };
    // `round`'s optional second argument is the multiple to snap to. Evaluated
    // as a literal rather than a series: MetricsQL allows an expression there,
    // but a non-constant nearest has no sensible per-sample meaning here and
    // silently varying it would be worse than refusing.
    let nearest = match (t, f.args.get(1)) {
        (TransformFn::Round, Some(Expr::Number(n))) => n.value,
        (TransformFn::Round, Some(_)) => {
            return Err(EvalError::NotYetImplemented(
                "round() with a non-literal `nearest` argument",
            ));
        }
        _ => 1.0,
    };
    if nearest == 0.0 || !nearest.is_finite() {
        return Err(EvalError::NotYetImplemented(
            "round() with a zero or non-finite `nearest` argument",
        ));
    }

    let mut series = evaluate(ctx, arg)?;
    for s in &mut series {
        for sample in &mut s.samples {
            let v = sample.value;
            sample.value = match t {
                // Divide-round-multiply so `nearest` works, and so plain
                // `round(x)` is exactly `x.round()`.
                TransformFn::Round => (v / nearest).round() * nearest,
                TransformFn::Abs => v.abs(),
                TransformFn::Ceil => v.ceil(),
                TransformFn::Floor => v.floor(),
            };
        }
    }
    Ok(series)
}

/// Dispatch [`FuncExpr`] by name. Aggregate functions
/// (sum/avg/min/max/count and friends) handle their `by` /
/// `without` modifiers here; transform / rollup functions
/// will land in subsequent passes.
fn evaluate_func(ctx: &EvalContext<'_>, f: &FuncExpr) -> Result<Vec<Series>, EvalError> {
    if f.name.is_empty() {
        return Err(EvalError::NotYetImplemented(
            "anonymous union() function (multi-element parens group)",
        ));
    }
    if let Some(pop) = ParameterizedAggregateOp::from_name(&f.name) {
        return evaluate_parameterized_aggregate(ctx, f, pop);
    }
    if let Some(agg) = AggregateOp::from_name(&f.name) {
        return evaluate_aggregate(ctx, f, agg);
    }
    if let Some(op) = RollupFn::from_name(&f.name) {
        return evaluate_rollup_fn(ctx, f, op);
    }
    // Parameterized rollup: `quantile_over_time(phi, vec[w])`
    // — 2-arg form. Lift it out of the 1-arg dispatch above.
    if f.name.eq_ignore_ascii_case("quantile_over_time") {
        return evaluate_quantile_over_time(ctx, f);
    }
    if let Some(t) = TransformFn::from_name(&f.name) {
        return evaluate_transform(ctx, f, t);
    }
    Err(EvalError::NotYetImplemented(
        "non-aggregate / non-rollup function calls",
    ))
}

/// Aggregates that take a leading scalar parameter:
/// - `topk(k, vec)` — keep top-K series per (group, ts).
/// - `bottomk(k, vec)` — keep bottom-K.
/// - `quantile(phi, vec)` — reduce to phi-quantile per (group, ts),
///   like avg/sum.
///
/// Differ from [`AggregateOp`] in two ways: the leading scalar
/// arg, and the labels-preserving behaviour of topk/bottomk
/// (they pick which series survive, not aggregate their values).
#[derive(Debug, Clone, Copy)]
enum ParameterizedAggregateOp {
    TopK,
    BottomK,
    Quantile,
}

impl ParameterizedAggregateOp {
    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "topk" => Some(Self::TopK),
            "bottomk" => Some(Self::BottomK),
            "quantile" => Some(Self::Quantile),
            _ => None,
        }
    }
}

fn evaluate_parameterized_aggregate(
    ctx: &EvalContext<'_>,
    f: &FuncExpr,
    op: ParameterizedAggregateOp,
) -> Result<Vec<Series>, EvalError> {
    if f.args.len() != 2 {
        return Err(EvalError::BadValue(format!(
            "{:?} expects 2 args (scalar, vector), got {}",
            f.name,
            f.args.len()
        )));
    }
    let Expr::Number(NumberExpr { value: param, .. }) = &f.args[0] else {
        return Err(EvalError::BadValue(format!(
            "{:?} first arg must be a numeric literal",
            f.name
        )));
    };
    let param = *param;
    if matches!(op, ParameterizedAggregateOp::Quantile) && !(0.0..=1.0).contains(&param) {
        return Err(EvalError::BadValue(format!(
            "quantile phi must be in [0, 1], got {param}"
        )));
    }
    let input = evaluate(ctx, &f.args[1])?;
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let groups = group_series(&input, f.modifier.as_ref());
    match op {
        ParameterizedAggregateOp::Quantile => {
            let mut out: Vec<Series> = Vec::with_capacity(groups.len());
            for (group_labels, members) in groups {
                out.push(reduce_group_quantile(group_labels, &members, param));
            }
            Ok(out)
        }
        ParameterizedAggregateOp::TopK | ParameterizedAggregateOp::BottomK => {
            let k = if param.is_nan() {
                0
            } else {
                param.max(0.0) as usize
            };
            let mut out: Vec<Series> = Vec::new();
            for (_group_labels, members) in groups {
                pick_k_per_timestamp(
                    &members,
                    k,
                    matches!(op, ParameterizedAggregateOp::BottomK),
                    &mut out,
                );
            }
            Ok(out)
        }
    }
}

/// Per-timestamp phi-quantile reducer. Linear interpolation
/// between adjacent values (matches upstream PromQL `quantile`,
/// which is distinct from the HDR-backed `quantile_over_time`).
fn reduce_group_quantile(
    group_labels: Vec<(String, String)>,
    members: &[Series],
    phi: f64,
) -> Series {
    let mut buckets: Vec<(i64, Vec<f64>)> = Vec::new();
    for s in members {
        for sample in &s.samples {
            if sample.value.is_nan() {
                continue;
            }
            match buckets.iter_mut().find(|(t, _)| *t == sample.timestamp_ms) {
                Some((_, vals)) => vals.push(sample.value),
                None => buckets.push((sample.timestamp_ms, vec![sample.value])),
            }
        }
    }
    buckets.sort_by_key(|(t, _)| *t);
    let samples: Vec<Sample> = buckets
        .into_iter()
        .map(|(t, mut vals)| {
            vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let value = quantile_linear(&vals, phi);
            Sample {
                timestamp_ms: t,
                value,
            }
        })
        .collect();
    Series {
        labels: group_labels,
        samples,
    }
}

/// Linear-interpolated phi-quantile over a sorted slice.
/// Matches upstream PromQL: rank = phi * (n - 1); interpolate
/// between floor and ceil. NaN for empty input; `phi >= 1` →
/// max; `phi <= 0` → min.
fn quantile_linear(sorted: &[f64], phi: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    if phi.is_nan() {
        return f64::NAN;
    }
    if phi <= 0.0 {
        return sorted[0];
    }
    if phi >= 1.0 {
        return sorted[sorted.len() - 1];
    }
    let rank = phi * (sorted.len() as f64 - 1.0);
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let frac = rank - lo as f64;
    sorted[lo] * (1.0 - frac) + sorted[hi] * frac
}

/// Pick the K largest (or smallest, when `bottom`) values per
/// timestamp across `members`, preserving the source labels of
/// each picked series. A series appears in the output only at
/// timestamps where it made the cut; ties are broken by source
/// order. Output series carry their full original labels — no
/// `__name__` strip — to match upstream's "select, don't
/// aggregate" topk/bottomk semantics.
fn pick_k_per_timestamp(members: &[Series], k: usize, bottom: bool, out: &mut Vec<Series>) {
    if k == 0 || members.is_empty() {
        return;
    }
    // Collect the union of timestamps.
    let mut timestamps: Vec<i64> = Vec::new();
    for m in members {
        for s in &m.samples {
            if !timestamps.contains(&s.timestamp_ms) {
                timestamps.push(s.timestamp_ms);
            }
        }
    }
    timestamps.sort();
    // For each timestamp, rank members and emit the top/bottom K.
    // member_idx → Vec<Sample> accumulator.
    let mut picked: Vec<(usize, Vec<Sample>)> = Vec::new();
    for ts in timestamps {
        let mut ranked: Vec<(usize, f64)> = members
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                m.samples
                    .iter()
                    .find(|s| s.timestamp_ms == ts && !s.value.is_nan())
                    .map(|s| (i, s.value))
            })
            .collect();
        ranked.sort_by(|a, b| {
            if bottom {
                a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
            } else {
                b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
            }
        });
        for (idx, val) in ranked.into_iter().take(k) {
            let entry = match picked.iter_mut().find(|(i, _)| *i == idx) {
                Some(e) => e,
                None => {
                    picked.push((idx, Vec::new()));
                    picked.last_mut().unwrap()
                }
            };
            entry.1.push(Sample {
                timestamp_ms: ts,
                value: val,
            });
        }
    }
    for (idx, samples) in picked {
        out.push(Series {
            labels: members[idx].labels.clone(),
            samples,
        });
    }
}

fn evaluate_quantile_over_time(
    ctx: &EvalContext<'_>,
    f: &FuncExpr,
) -> Result<Vec<Series>, EvalError> {
    if f.args.len() != 2 {
        return Err(EvalError::BadValue(format!(
            "quantile_over_time expects 2 args, got {}",
            f.args.len()
        )));
    }
    let Expr::Number(NumberExpr { value: phi, .. }) = &f.args[0] else {
        return Err(EvalError::BadValue(
            "quantile_over_time first arg must be a numeric quantile".into(),
        ));
    };
    let phi = *phi;
    if !(0.0..=1.0).contains(&phi) {
        return Err(EvalError::BadValue(format!(
            "quantile_over_time phi must be in [0, 1], got {phi}"
        )));
    }
    let arg = &f.args[1];
    let _window_ms: Option<i64> = window_of_arg(arg, ctx.step_ms)?;
    let input = evaluate(ctx, arg)?;
    let mut out: Vec<Series> = Vec::with_capacity(input.len());
    for s in input {
        let value = quantile_via_hdr(&s.samples, phi);
        let labels = labels_after_op(&s.labels);
        out.push(Series {
            labels,
            samples: vec![Sample {
                timestamp_ms: ctx.end_ms,
                value,
            }],
        });
    }
    Ok(out)
}

/// Compute a phi-quantile via HDR histogram. Same precision
/// choice as the streaming reducer so batch and streaming
/// land on the same value, exactly. Negative / NaN samples
/// are skipped; values are floored to `u64`.
fn quantile_via_hdr(samples: &[Sample], phi: f64) -> f64 {
    let mut hist = hdrhistogram::Histogram::<u64>::new_with_bounds(1, 1_000_000_000_000, 3)
        .expect("HDR construction");
    for s in samples {
        if s.value.is_nan() || s.value < 0.0 {
            continue;
        }
        let v = (s.value.floor().min(1_000_000_000_000.0) as u64).max(1);
        let _ = hist.record(v);
    }
    if hist.is_empty() {
        return f64::NAN;
    }
    hist.value_at_quantile(phi) as f64
}

/// Rollup-function reducers: take a range-vector argument
/// (`metric[w]`), reduce each series's window down to a
/// single value at the query anchor. Mirrors the subset of
/// upstream's `rollupFuncs` that nmbrs's reporting actually
/// reaches for first; the long tail (quantile, holt-winters,
/// stddev_over_time, …) lands as queries demand it.
#[derive(Debug, Clone, Copy)]
enum RollupFn {
    Rate,
    Increase,
    Delta,
    SumOverTime,
    AvgOverTime,
    MinOverTime,
    MaxOverTime,
    CountOverTime,
    LastOverTime,
    FirstOverTime,
    /// Unix timestamp (SECONDS, as MetricsQL returns) of the first / last sample in
    /// the window, rather than its value. Lets a table show WHEN a series started
    /// and last moved, and lets an IN-FLIGHT phase report elapsed time as
    /// `tlast - tfirst`: a phase that never completes has no duration metric to
    /// read, because those are written at completion.
    TFirstOverTime,
    TLastOverTime,
    /// Unix seconds of the last sample whose value DIFFERS from the one before it.
    /// Metric instances keep being sampled at their last value after the work that
    /// fed them stops, so a sample's existence says nothing about activity — the
    /// value moving does. Paired with `tfirst_over_time` (which lands on the
    /// instance's first emission, i.e. when the work began) this bounds the window
    /// in which a series was actually doing something; for a phase still running
    /// that window ends at "just now" and keeps growing.
    TLastChangeOverTime,
    StddevOverTime,
    StdvarOverTime,
}

impl RollupFn {
    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "rate" => Some(Self::Rate),
            "increase" => Some(Self::Increase),
            "delta" => Some(Self::Delta),
            "sum_over_time" => Some(Self::SumOverTime),
            "avg_over_time" => Some(Self::AvgOverTime),
            "min_over_time" => Some(Self::MinOverTime),
            "max_over_time" => Some(Self::MaxOverTime),
            "count_over_time" => Some(Self::CountOverTime),
            "last_over_time" => Some(Self::LastOverTime),
            "first_over_time" => Some(Self::FirstOverTime),
            "tfirst_over_time" => Some(Self::TFirstOverTime),
            "tlast_over_time" => Some(Self::TLastOverTime),
            "tlast_change_over_time" => Some(Self::TLastChangeOverTime),
            "stddev_over_time" => Some(Self::StddevOverTime),
            "stdvar_over_time" => Some(Self::StdvarOverTime),
            _ => None,
        }
    }

    /// Whether the reducer needs the window length in seconds
    /// (to compute a per-second rate). Other reducers ignore
    /// it.
    fn needs_window(self) -> bool {
        matches!(self, Self::Rate)
    }

    /// Whether the reducer is a **cumulative-counter difference** over the
    /// window (`rate`/`increase`) and so wants the pre-window *bracketing*
    /// sample — the counter's value at the window start — fetched via a widened
    /// lookback. See [`bracket_rate`].
    fn brackets(self) -> bool {
        matches!(self, Self::Rate | Self::Increase)
    }
}

/// How far before a `rate`/`increase` window to look for the **bracketing**
/// sample (the cumulative value at the window start). Generous on purpose: a
/// window narrower than the sample cadence (the servo's `[400ms]` against a
/// 1 s cadence) has the prior cumulative point up to one cadence before the
/// window, and tick jitter can stretch that — `max(window, this)` catches it
/// without an unbounded scan (the in-memory horizon bounds the fetch anyway).
const BRACKET_LOOKBACK_FLOOR_MS: i64 = 2_000;

/// Evaluate a rollup function over a range-vector argument.
/// The evaluator only accepts an explicit `[w]` window today;
/// PromQL's "infer window from query step" shorthand
/// (`rate(cpu)` without brackets) lands when subqueries do
/// — that's where step-relative durations get their proper
/// hookup.
fn evaluate_rollup_fn(
    ctx: &EvalContext<'_>,
    f: &FuncExpr,
    op: RollupFn,
) -> Result<Vec<Series>, EvalError> {
    if f.args.len() != 1 {
        return Err(EvalError::BadValue(format!(
            "rollup function {:?} expects 1 range-vector argument, got {}",
            f.name,
            f.args.len()
        )));
    }
    let arg = &f.args[0];
    let window_ms: Option<i64> = window_of_arg(arg, ctx.step_ms)?;
    if op.needs_window() && window_ms.is_none() {
        return Err(EvalError::BadValue(format!(
            "rollup function {:?} needs an explicit `[window]` argument",
            f.name
        )));
    }
    // For rate/increase the [w] window pins the reduction span. The selector's
    // anchor for a rollup is `ctx.end_ms` after any `offset` has been applied
    // upstream — see `evaluate_rollup` for that bookkeeping.
    let range_end_ms = ctx.end_ms;
    let range_start_ms = range_end_ms - window_ms.unwrap_or(0);
    let input = if op.brackets() {
        // Bracket-rate (cumulative-native): widen the fetch to `[range_start −
        // lookback, range_end]` so a sample at/before the window start anchors
        // the left edge. `bracket_rate` then differences `cum[range_end] −
        // cum[range_start]` from the bracketing samples — no ≥2-in-window
        // requirement, so a window narrower than the cadence (or thinned by
        // tick jitter) still yields a stable value instead of NaN. The inner
        // *selector* is fetched directly (not the `[w]` rollup) over the widened
        // range; `range_start_ms` still marks the true window edge for the
        // reduction.
        let bracket_lookback = window_ms.unwrap_or(0).max(BRACKET_LOOKBACK_FLOOR_MS);
        let inner: &Expr = match arg {
            Expr::Rollup(re) => re.expr.as_ref(),
            other => other,
        };
        let bracket_ctx = EvalContext {
            data: ctx.data,
            start_ms: range_start_ms - bracket_lookback,
            end_ms: range_end_ms,
            step_ms: ctx.step_ms,
            lookback_ms: ctx.lookback_ms,
            query_start_ms: ctx.query_start_ms,
            query_end_ms: ctx.query_end_ms,
        };
        evaluate(&bracket_ctx, inner)?
    } else {
        evaluate(ctx, arg)?
    };
    let mut out: Vec<Series> = Vec::with_capacity(input.len());
    for s in input {
        let value = reduce_rollup(
            op,
            &s.samples,
            window_ms.unwrap_or(0),
            range_start_ms,
            range_end_ms,
        );
        let labels = labels_after_op(&s.labels);
        out.push(Series {
            labels,
            samples: vec![Sample {
                timestamp_ms: ctx.end_ms,
                value,
            }],
        });
    }
    Ok(out)
}

/// Pick the window duration off a rollup argument's outer
/// `RollupExpr`. `None` when no window is present (the
/// argument was a bare selector — only valid for reducers
/// that don't need the window length).
fn window_of_arg(arg: &Expr, step_ms: i64) -> Result<Option<i64>, EvalError> {
    if let Expr::Rollup(re) = arg
        && let Some(w) = &re.window
    {
        return Ok(Some(parse_duration_ms(&w.value, step_ms)?));
    }
    Ok(None)
}

/// Apply the rollup reducer to one series's windowed samples.
/// NaN samples are dropped before reducing, per upstream;
/// empty inputs produce NaN.
///
/// `range_start_ms`/`range_end_ms` pin the `[w]` window's
/// anchor frame; rate/increase use them to extrapolate when
/// the first/last samples don't sit at the edges. Other
/// reducers ignore them.
fn reduce_rollup(
    op: RollupFn,
    samples: &[Sample],
    window_ms: i64,
    range_start_ms: i64,
    range_end_ms: i64,
) -> f64 {
    let xs: Vec<&Sample> = samples.iter().filter(|s| !s.value.is_nan()).collect();
    if xs.is_empty() {
        return f64::NAN;
    }
    match op {
        RollupFn::Rate => {
            // Cumulative-native: reset-adjusted counter increase across the
            // window via the bracketing samples, divided by window seconds.
            if window_ms == 0 {
                return f64::NAN;
            }
            bracket_rate(&xs, window_ms, range_start_ms, range_end_ms, true)
        }
        RollupFn::Increase => {
            // Same, without dividing by seconds — the absolute increase.
            bracket_rate(&xs, window_ms, range_start_ms, range_end_ms, false)
        }
        RollupFn::Delta => {
            // Gauge delta: no counter-reset adjustment, no
            // extrapolation — `delta` is for non-monotonic
            // series and any "negative jump" is a real signal.
            let first = xs.first().unwrap().value;
            let last = xs.last().unwrap().value;
            last - first
        }
        RollupFn::SumOverTime => xs.iter().map(|s| s.value).sum(),
        RollupFn::AvgOverTime => xs.iter().map(|s| s.value).sum::<f64>() / xs.len() as f64,
        RollupFn::MinOverTime => xs.iter().map(|s| s.value).fold(f64::INFINITY, f64::min),
        RollupFn::MaxOverTime => xs.iter().map(|s| s.value).fold(f64::NEG_INFINITY, f64::max),
        RollupFn::CountOverTime => xs.len() as f64,
        RollupFn::LastOverTime => xs.last().unwrap().value,
        RollupFn::FirstOverTime => xs.first().unwrap().value,
        // Samples arrive ordered by time, so first/last are the window's edges.
        // Seconds, not milliseconds: that is what MetricsQL's t*_over_time return,
        // and it makes `tlast - tfirst` read directly as elapsed seconds.
        RollupFn::TFirstOverTime => xs.first().unwrap().timestamp_ms as f64 / 1000.0,
        RollupFn::TLastOverTime => xs.last().unwrap().timestamp_ms as f64 / 1000.0,
        RollupFn::TLastChangeOverTime => xs
            .windows(2)
            .filter(|w| w[1].value != w[0].value)
            .next_back()
            .map(|w| w[1].timestamp_ms as f64 / 1000.0)
            .unwrap_or(xs.first().unwrap().timestamp_ms as f64 / 1000.0),
        RollupFn::StddevOverTime => {
            let vals: Vec<f64> = xs.iter().map(|s| s.value).collect();
            population_variance(&vals).sqrt()
        }
        RollupFn::StdvarOverTime => {
            let vals: Vec<f64> = xs.iter().map(|s| s.value).collect();
            population_variance(&vals)
        }
    }
}

/// Cumulative-counter–native rate / increase: the reset-adjusted increase of a
/// monotonic counter across the window `[range_start_ms, range_end_ms]`,
/// computed by differencing the counter's value at the window edges using the
/// samples that **bracket** them.
///
/// - **Left edge** = the counter value at the window start = the last sample at
///   or before `range_start_ms` (the bracketing sample fetched via the widened
///   lookback in `evaluate_rollup_fn`). If none precedes the window, fall back
///   to the first available sample (the counter "begins" inside the window).
/// - **Right edge** = the last sample at or before `range_end_ms`.
/// - **Counter resets:** any `xs[i] < xs[i-1]` between the edges is a wrap; add
///   `xs[i-1]` back so the result reflects the underlying monotonic counter.
///
/// Unlike Prometheus extrapolation this needs no ≥2 *in-window* samples — one
/// bracketing sample on each side suffices — so a window narrower than the
/// sample cadence, or one thinned by tick jitter under concurrency, yields a
/// **stable, deterministic** value instead of NaN. When the two edges resolve
/// to the same sample (no change observed across the window) the increase is
/// `0`, which is the correct reading for a counter that didn't advance.
/// `is_rate` divides by the window's seconds.
fn bracket_rate(
    xs: &[&Sample],
    window_ms: i64,
    range_start_ms: i64,
    range_end_ms: i64,
    is_rate: bool,
) -> f64 {
    // `xs` is ascending by timestamp (the fetch returns sorted samples).
    let Some(right_idx) = xs.iter().rposition(|s| s.timestamp_ms <= range_end_ms) else {
        return f64::NAN; // nothing in or before the window
    };
    // The bracketing left edge: last sample at/before the window start, else
    // the earliest sample (counter started inside the window).
    let left_idx = xs
        .iter()
        .rposition(|s| s.timestamp_ms <= range_start_ms)
        .unwrap_or(0);
    if left_idx >= right_idx {
        // One effective sample across the window → no observed change.
        return 0.0;
    }
    let mut increase = xs[right_idx].value - xs[left_idx].value;
    for i in (left_idx + 1)..=right_idx {
        if xs[i].value < xs[i - 1].value {
            increase += xs[i - 1].value; // counter reset / wrap
        }
    }
    if is_rate {
        increase / (window_ms as f64 / 1000.0)
    } else {
        increase
    }
}

/// Aggregate function kinds the evaluator implements today.
/// Mirrors the subset of upstream `aggrFuncs` that take a
/// single vector argument and reduce per-group; the
/// percentile / histogram aggregates land later.
#[derive(Debug, Clone, Copy)]
enum AggregateOp {
    Sum,
    Avg,
    Min,
    Max,
    Count,
    Group,
    Stddev,
    Stdvar,
}

impl AggregateOp {
    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "sum" => Some(Self::Sum),
            "avg" => Some(Self::Avg),
            "min" => Some(Self::Min),
            "max" => Some(Self::Max),
            "count" => Some(Self::Count),
            "group" => Some(Self::Group),
            "stddev" => Some(Self::Stddev),
            "stdvar" => Some(Self::Stdvar),
            _ => None,
        }
    }
}

fn evaluate_aggregate(
    ctx: &EvalContext<'_>,
    f: &FuncExpr,
    op: AggregateOp,
) -> Result<Vec<Series>, EvalError> {
    if f.args.len() != 1 {
        return Err(EvalError::BadValue(format!(
            "aggregate {:?} expects 1 vector argument, got {}",
            f.name,
            f.args.len()
        )));
    }
    let input = evaluate(ctx, &f.args[0])?;
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let groups = group_series(&input, f.modifier.as_ref());
    let mut out: Vec<Series> = Vec::with_capacity(groups.len());
    for (group_labels, members) in groups {
        out.push(reduce_group(op, group_labels, &members));
    }
    Ok(out)
}

/// One aggregation bucket: its (modifier-filtered) group labels
/// paired with the member series that fall into it.
type SeriesGroup = (Vec<(String, String)>, Vec<Series>);

/// Bucket the input series by their (modifier-filtered)
/// label sets. Returns `(group_labels, members)` pairs in
/// first-encountered order; with no modifier, all series
/// collapse into a single empty-labelled group (per upstream
/// aggregate semantics).
fn group_series(input: &[Series], modifier: Option<&AggrModifier>) -> Vec<SeriesGroup> {
    let mut out: Vec<SeriesGroup> = Vec::new();
    for s in input {
        let key = group_key(&s.labels, modifier);
        match out.iter_mut().find(|(k, _)| label_sets_equal(k, &key)) {
            Some((_, members)) => members.push(s.clone()),
            None => out.push((key, vec![s.clone()])),
        }
    }
    out
}

/// Project a series's label set down to the labels the
/// aggregate's modifier preserves. Mirrors upstream:
/// - `by(l1, l2)` → keep only l1, l2.
/// - `without(l1, l2)` → keep everything except l1, l2 and
///   `__name__` (always dropped).
/// - no modifier → empty key (all series share one bucket).
fn group_key(
    labels: &[(String, String)],
    modifier: Option<&AggrModifier>,
) -> Vec<(String, String)> {
    let Some(m) = modifier else {
        return Vec::new();
    };
    let mut keep: Vec<(String, String)> = match m.op {
        AggrModifierOp::By => labels
            .iter()
            .filter(|(k, _)| m.args.iter().any(|w| w == k))
            .cloned()
            .collect(),
        AggrModifierOp::Without => labels
            .iter()
            .filter(|(k, _)| k != "__name__" && !m.args.iter().any(|w| w == k))
            .cloned()
            .collect(),
    };
    keep.sort_by(|a, b| a.0.cmp(&b.0));
    keep
}

/// Apply the aggregate operator across the timestamp-aligned
/// samples in `members` and emit a single [`Series`] tagged
/// with `group_labels`. Samples with NaN values are skipped
/// per Prometheus' aggregate semantics.
/// Population variance — `Σ(x - mean)² / n`. Returns NaN
/// for empty input (matching `Sum`'s "no data" snapshot)
/// and `0.0` for a single sample. Matches upstream PromQL's
/// `stdvar` semantics.
fn population_variance(vals: &[f64]) -> f64 {
    if vals.is_empty() {
        return f64::NAN;
    }
    let mean = vals.iter().sum::<f64>() / vals.len() as f64;
    let m2: f64 = vals.iter().map(|v| (v - mean) * (v - mean)).sum();
    m2 / vals.len() as f64
}

fn reduce_group(
    op: AggregateOp,
    group_labels: Vec<(String, String)>,
    members: &[Series],
) -> Series {
    // Bucket samples by timestamp first, then reduce each
    // bucket. We don't assume members share a timestamp set —
    // missing samples just don't contribute to that bucket.
    let mut buckets: Vec<(i64, Vec<f64>)> = Vec::new();
    for s in members {
        for sample in &s.samples {
            if sample.value.is_nan() {
                continue;
            }
            match buckets.iter_mut().find(|(t, _)| *t == sample.timestamp_ms) {
                Some((_, vals)) => vals.push(sample.value),
                None => buckets.push((sample.timestamp_ms, vec![sample.value])),
            }
        }
    }
    buckets.sort_by_key(|(t, _)| *t);
    let samples: Vec<Sample> = buckets
        .into_iter()
        .map(|(t, vals)| {
            let value = match op {
                AggregateOp::Sum => vals.iter().sum::<f64>(),
                AggregateOp::Avg => vals.iter().sum::<f64>() / vals.len() as f64,
                AggregateOp::Min => vals.iter().cloned().fold(f64::INFINITY, f64::min),
                AggregateOp::Max => vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                AggregateOp::Count => vals.len() as f64,
                AggregateOp::Group => 1.0,
                // Population variance / stddev — matches
                // upstream PromQL semantics. NaN for n < 1 (no
                // data) and 0 for n == 1 (single sample).
                AggregateOp::Stdvar => population_variance(&vals),
                AggregateOp::Stddev => population_variance(&vals).sqrt(),
            };
            Sample {
                timestamp_ms: t,
                value,
            }
        })
        .collect();
    Series {
        labels: group_labels,
        samples,
    }
}

/// Evaluate a [`RollupExpr`]: shift / window the time range
/// the inner expression observes.
///
/// For an instant query at time `T`:
/// - `m[5m]` → fetch series with samples in `[T-5m, T]`.
/// - `m offset 1h` → instant value at `T-1h` (range
///   `[T-1h, T-1h]`).
/// - `m[5m] offset 1h` → window `[T-1h-5m, T-1h]`.
/// - `m @ 1500000000` → anchor at the literal seconds timestamp
///   instead of `T`.
///
/// `step` (sub-query step) and non-literal `@` expressions
/// (`start()`, `end()`) are deferred — they only matter once
/// range queries land.
fn evaluate_rollup(ctx: &EvalContext<'_>, re: &RollupExpr) -> Result<Vec<Series>, EvalError> {
    let mut anchor_end_ms = ctx.end_ms;
    if let Some(at) = &re.at {
        anchor_end_ms = evaluate_at_modifier(ctx, at)?;
    }
    if let Some(off) = &re.offset {
        anchor_end_ms -= duration_to_ms(off, ctx.step_ms)?;
    }
    let anchor_start_ms = if let Some(w) = &re.window {
        anchor_end_ms - duration_to_ms(w, ctx.step_ms)?
    } else {
        // No window — instant lookup at the (offset-adjusted)
        // anchor. Inner expression sees a degenerate range.
        anchor_end_ms
    };
    // Subquery: `expr[w:s]` evaluates `expr` at each sub-step
    // within `[anchor_start, anchor_end]`, stepping by `s`.
    // The result is a synthetic range-vector that the outer
    // rollup function reduces. `inherit_step` (`expr[w:]`)
    // borrows the outer step_ms.
    if re.step.is_some() || re.inherit_step {
        let sub_step_ms = if re.inherit_step {
            ctx.step_ms
        } else {
            duration_to_ms(re.step.as_ref().unwrap(), ctx.step_ms)?
        };
        if sub_step_ms <= 0 {
            return Err(EvalError::BadValue(format!(
                "subquery step must be > 0, got {sub_step_ms}ms"
            )));
        }
        let inner_range = EvalContext {
            data: ctx.data,
            start_ms: anchor_start_ms,
            end_ms: anchor_end_ms,
            step_ms: sub_step_ms,
            lookback_ms: ctx.lookback_ms,
            query_start_ms: ctx.query_start_ms.or(Some(ctx.start_ms)),
            query_end_ms: ctx.query_end_ms.or(Some(ctx.end_ms)),
        };
        return evaluate_range(&inner_range, &re.expr);
    }
    let inner_ctx = EvalContext {
        data: ctx.data,
        start_ms: anchor_start_ms,
        end_ms: anchor_end_ms,
        step_ms: ctx.step_ms,
        // The rollup's window IS the lookback; for windowed
        // rollups (`metric[5m]`) the inner ctx has
        // `start != end` so the selector returns all
        // samples in window. For windowless rollups
        // (`metric offset 5m`) the inner ctx is instant —
        // propagating the outer lookback is correct.
        lookback_ms: ctx.lookback_ms,
        query_start_ms: ctx.query_start_ms,
        query_end_ms: ctx.query_end_ms,
    };
    evaluate(&inner_ctx, &re.expr)
}

/// Resolve the `@ <expr>` modifier to a millisecond timestamp.
///   - numeric literal — Unix seconds (matches upstream's
///     `@ <ts>` syntax).
///   - `start()` / `end()` — pin to the outer query window.
///     Defaults to `ctx.start_ms` / `ctx.end_ms` when the
///     query bounds aren't explicitly set (instant queries).
fn evaluate_at_modifier(ctx: &EvalContext<'_>, at: &Expr) -> Result<i64, EvalError> {
    match at {
        Expr::Number(NumberExpr { value, .. }) => Ok((*value * 1000.0) as i64),
        Expr::Func(f) if f.args.is_empty() => match f.name.to_ascii_lowercase().as_str() {
            "start" => Ok(ctx.query_start_ms.unwrap_or(ctx.start_ms)),
            "end" => Ok(ctx.query_end_ms.unwrap_or(ctx.end_ms)),
            other => Err(EvalError::BadValue(format!(
                "`@` only accepts start() / end() / <unix-ts>; got {other}()"
            ))),
        },
        _ => Err(EvalError::BadValue(
            "`@` modifier must be a numeric timestamp or start()/end()".into(),
        )),
    }
}

/// Parse a [`DurationExpr`] to milliseconds. Mirrors upstream
/// `metricsql.PositiveDuration` for the unit set the
/// evaluator currently exercises (s/m/h/d/w/y, ms, plus
/// step-relative `i`). Multi-unit durations like `1h30m` and
/// the leading `-` sign for offsets are both supported.
fn duration_to_ms(d: &DurationExpr, step_ms: i64) -> Result<i64, EvalError> {
    parse_duration_ms(&d.value, step_ms)
}

fn parse_duration_ms(s: &str, step_ms: i64) -> Result<i64, EvalError> {
    let (sign, rest) = if let Some(stripped) = s.strip_prefix('-') {
        (-1i64, stripped)
    } else {
        (1i64, s)
    };
    if rest.is_empty() {
        return Err(EvalError::BadValue(format!("empty duration {s:?}")));
    }
    // Bare numeric → seconds (matches upstream's
    // `PositiveDurationValue` fallback).
    if let Ok(n) = rest.parse::<f64>() {
        return Ok(sign * (n * 1000.0) as i64);
    }
    let bytes = rest.as_bytes();
    let mut i = 0;
    let mut total_ms: i64 = 0;
    while i < bytes.len() {
        // Mantissa: optional digits + optional `.digits`.
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        if i == start {
            return Err(EvalError::BadValue(format!(
                "duration {s:?}: expected number at byte {start}"
            )));
        }
        let n: f64 = rest[start..i].parse().map_err(|_| {
            EvalError::BadValue(format!(
                "duration {s:?}: invalid number {:?}",
                &rest[start..i]
            ))
        })?;
        // Unit: 1- or 2-letter suffix.
        let unit_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        if i == unit_start {
            return Err(EvalError::BadValue(format!(
                "duration {s:?}: missing unit after {n}"
            )));
        }
        let unit = &rest[unit_start..i];
        let unit_ms = match unit {
            "ms" => 1.0,
            "s" => 1_000.0,
            "m" => 60.0 * 1_000.0,
            "h" => 60.0 * 60.0 * 1_000.0,
            "d" => 24.0 * 60.0 * 60.0 * 1_000.0,
            "w" => 7.0 * 24.0 * 60.0 * 60.0 * 1_000.0,
            "y" => 365.0 * 24.0 * 60.0 * 60.0 * 1_000.0,
            "i" => step_ms as f64,
            other => {
                return Err(EvalError::BadValue(format!(
                    "duration {s:?}: unknown unit {other:?}"
                )));
            }
        };
        total_ms = total_ms.saturating_add((n * unit_ms) as i64);
    }
    Ok(sign * total_ms)
}

fn filters_to_matchers(group: &[LabelFilter]) -> Result<Vec<Matcher>, EvalError> {
    let mut out = Vec::with_capacity(group.len());
    for lf in group {
        if lf.is_template_ref {
            return Err(EvalError::InvalidShape(format!(
                "unexpanded template-ref filter {:?} in selector — use `parse` not `parse_for_prettify`",
                lf.label
            )));
        }
        if lf.value_expr.is_some() {
            return Err(EvalError::InvalidShape(format!(
                "unexpanded value expression on filter {:?} in selector",
                lf.label
            )));
        }
        out.push(Matcher {
            label: lf.label.clone(),
            op: match lf.op {
                LabelFilterOp::Eq => MatcherOp::Eq,
                LabelFilterOp::Ne => MatcherOp::Ne,
                LabelFilterOp::EqRegex => MatcherOp::EqRegex,
                LabelFilterOp::NeRegex => MatcherOp::NeRegex,
            },
            value: lf.value.clone(),
        });
    }
    Ok(out)
}

pub(crate) fn label_sets_equal(a: &[(String, String)], b: &[(String, String)]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for (k, v) in a {
        if !b.iter().any(|(k2, v2)| k2 == k && v2 == v) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    /// In-memory backend that exact-matches `Eq` matchers and
    /// supports `Ne`. Regex matchers fall back to a literal
    /// equality check (the evaluator path under test doesn't
    /// exercise them yet).
    struct MemoryDataSource {
        series: Vec<Series>,
    }

    impl MetricAccess for MemoryDataSource {
        fn select_range(
            &self,
            matchers: &[Matcher],
            _start: i64,
            _end: i64,
        ) -> Result<Vector, DataSourceError> {
            Ok(self
                .series
                .iter()
                .filter(|s| matchers.iter().all(|m| matches_series(m, s)))
                .cloned()
                .collect())
        }
    }

    fn matches_series(m: &Matcher, s: &Series) -> bool {
        let v = s
            .labels
            .iter()
            .find(|(k, _)| k == &m.label)
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        match m.op {
            MatcherOp::Eq => v == m.value,
            MatcherOp::Ne => v != m.value,
            MatcherOp::EqRegex | MatcherOp::NeRegex => v == m.value,
        }
    }

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

    fn ctx_for(ds: &MemoryDataSource) -> EvalContext<'_> {
        EvalContext {
            data: ds,
            start_ms: 0,
            end_ms: 100,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        }
    }

    #[test]
    fn bare_metric_selector_returns_all_with_that_name() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "cpu"), ("host", "a")], &[(0, 1.0)]),
                series(&[("__name__", "cpu"), ("host", "b")], &[(0, 2.0)]),
                series(&[("__name__", "mem"), ("host", "a")], &[(0, 3.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        let ast = parse("cpu").expect("parse");
        let got = evaluate(&ctx, &ast).expect("eval");
        assert_eq!(got.len(), 2);
        assert!(
            got.iter()
                .any(|s| s.labels.iter().any(|(k, v)| k == "host" && v == "a"))
        );
        assert!(
            got.iter()
                .any(|s| s.labels.iter().any(|(k, v)| k == "host" && v == "b"))
        );
    }

    #[test]
    fn label_filter_narrows_selection() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "cpu"), ("host", "a")], &[(0, 1.0)]),
                series(&[("__name__", "cpu"), ("host", "b")], &[(0, 2.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        let ast = parse(r#"cpu{host="b"}"#).expect("parse");
        let got = evaluate(&ctx, &ast).expect("eval");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].samples[0].value, 2.0);
    }

    #[test]
    fn or_groups_union_results() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "cpu"), ("host", "a")], &[(0, 1.0)]),
                series(&[("__name__", "cpu"), ("host", "b")], &[(0, 2.0)]),
                series(&[("__name__", "cpu"), ("host", "c")], &[(0, 3.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        let ast = parse(r#"cpu{host="a" or host="b"}"#).expect("parse");
        let got = evaluate(&ctx, &ast).expect("eval");
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn or_groups_dedup_overlapping_series() {
        let ds = MemoryDataSource {
            series: vec![series(
                &[("__name__", "cpu"), ("host", "a"), ("zone", "z1")],
                &[(0, 1.0)],
            )],
        };
        let ctx = ctx_for(&ds);
        // Both sides match the same series — should appear once.
        let ast = parse(r#"cpu{host="a" or zone="z1"}"#).expect("parse");
        let got = evaluate(&ctx, &ast).expect("eval");
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn empty_selector_returns_nothing() {
        let ds = MemoryDataSource {
            series: vec![series(&[("__name__", "cpu")], &[(0, 1.0)])],
        };
        let ctx = ctx_for(&ds);
        let ast = parse("{}").expect("parse");
        let got = evaluate(&ctx, &ast).expect("eval");
        assert_eq!(got.len(), 0);
    }

    #[test]
    fn instant_lookback_picks_latest_sample_in_window() {
        let ds = WindowedDataSource {
            series: vec![series(
                &[("__name__", "cpu"), ("host", "h1")],
                &[(50, 1.0), (90, 2.0), (95, 3.0)],
            )],
        };
        // Anchor at 100, lookback 30ms — window is [70, 100],
        // so samples at 90 and 95 are visible; the latest
        // (95 → value 3.0) wins. The sample at 50 is
        // outside the window and ignored.
        let ctx = EvalContext {
            data: &ds,
            start_ms: 100,
            end_ms: 100,
            step_ms: 1,
            lookback_ms: Some(30),
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate(&ctx, &parse("cpu").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].samples.len(), 1);
        assert_eq!(got[0].samples[0].value, 3.0);
        // Sample is projected to the query anchor, not its
        // own timestamp.
        assert_eq!(got[0].samples[0].timestamp_ms, 100);
    }

    #[test]
    fn instant_lookback_drops_series_with_no_samples_in_window() {
        let ds = WindowedDataSource {
            series: vec![
                series(&[("__name__", "cpu"), ("host", "h1")], &[(0, 1.0)]),
                series(&[("__name__", "cpu"), ("host", "h2")], &[(50, 2.0)]),
            ],
        };
        // Window [70, 100] — only h2's sample is at 50, also
        // outside. h1's at 0 outside. Both drop.
        let ctx = EvalContext {
            data: &ds,
            start_ms: 100,
            end_ms: 100,
            step_ms: 1,
            lookback_ms: Some(30),
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate(&ctx, &parse("cpu").expect("parse")).expect("eval");
        assert!(got.is_empty());
    }

    #[test]
    fn instant_lookback_does_not_apply_inside_rollup() {
        let ds = WindowedDataSource {
            series: vec![series(
                &[("__name__", "cpu"), ("host", "h1")],
                &[(0, 0.0), (30_000, 50.0), (60_000, 100.0)],
            )],
        };
        // `rate(cpu[60s])` at instant T=60_000. Inside the
        // rollup, ctx becomes [0, 60_000] (start != end), so
        // the instant lookback should NOT collapse the
        // window down to one sample. The rollup must see
        // all three samples to compute (last - first) /
        // window_seconds correctly.
        let ctx = EvalContext {
            data: &ds,
            start_ms: 60_000,
            end_ms: 60_000,
            step_ms: 1,
            lookback_ms: Some(5_000), // 5s lookback, narrower than rollup window
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate(&ctx, &parse("rate(cpu[60s])").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        // Rate over 60s with a 100-unit increase → 100/60.
        assert!((got[0].samples[0].value - (100.0 / 60.0)).abs() < 1e-9);
    }

    #[test]
    fn lookback_none_preserves_strict_instant_semantics() {
        let ds = WindowedDataSource {
            series: vec![series(
                &[("__name__", "cpu"), ("host", "h1")],
                &[(50, 1.0), (95, 2.0)],
            )],
        };
        // Strict instant: query at T=100, no lookback. Data
        // source returns nothing for [100, 100], so result
        // is empty — current pre-lookback behaviour
        // preserved when caller passes `lookback_ms: None`.
        let ctx = EvalContext {
            data: &ds,
            start_ms: 100,
            end_ms: 100,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate(&ctx, &parse("cpu").expect("parse")).expect("eval");
        assert!(got.is_empty());
    }

    #[test]
    fn unimplemented_node_types_surface_cleanly() {
        let ds = MemoryDataSource { series: vec![] };
        let ctx = ctx_for(&ds);
        // String literals aren't wired yet — should not panic,
        // just report the gap.
        let ast = parse("\"hello\"").expect("parse");
        let err = evaluate(&ctx, &ast).expect_err("eval");
        assert!(matches!(err, EvalError::NotYetImplemented(_)));
    }

    /// In-memory source that records the time range it was
    /// last invoked with — lets tests assert that the rollup
    /// evaluator passed the right window.
    struct RecordingDataSource {
        series: Vec<Series>,
        // Mutex (not Cell): MetricAccess is a Send+Sync service.
        last_range: std::sync::Mutex<(i64, i64)>,
    }

    impl MetricAccess for RecordingDataSource {
        fn select_range(
            &self,
            matchers: &[Matcher],
            start: i64,
            end: i64,
        ) -> Result<Vector, DataSourceError> {
            *self.last_range.lock().unwrap() = (start, end);
            Ok(self
                .series
                .iter()
                .filter(|s| matchers.iter().all(|m| matches_series(m, s)))
                .cloned()
                .collect())
        }
    }

    #[test]
    fn rollup_window_shifts_start() {
        let ds = RecordingDataSource {
            series: vec![series(&[("__name__", "cpu")], &[(0, 1.0)])],
            last_range: std::sync::Mutex::new((-1, -1)),
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 1_000_000,
            end_ms: 1_000_000,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let ast = parse("cpu[5m]").expect("parse");
        evaluate(&ctx, &ast).expect("eval");
        let (start, end) = *ds.last_range.lock().unwrap();
        // 5m = 300_000 ms.
        assert_eq!(end, 1_000_000);
        assert_eq!(start, 1_000_000 - 300_000);
    }

    #[test]
    fn offset_shifts_anchor_back() {
        let ds = RecordingDataSource {
            series: vec![series(&[("__name__", "cpu")], &[(0, 1.0)])],
            last_range: std::sync::Mutex::new((-1, -1)),
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 1_000_000,
            end_ms: 1_000_000,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let ast = parse("cpu offset 1h").expect("parse");
        evaluate(&ctx, &ast).expect("eval");
        let (start, end) = *ds.last_range.lock().unwrap();
        // 1h = 3_600_000 ms; no window so start == end.
        assert_eq!(end, 1_000_000 - 3_600_000);
        assert_eq!(start, 1_000_000 - 3_600_000);
    }

    #[test]
    fn window_and_offset_compose() {
        let ds = RecordingDataSource {
            series: vec![series(&[("__name__", "cpu")], &[(0, 1.0)])],
            last_range: std::sync::Mutex::new((-1, -1)),
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 1_000_000,
            end_ms: 1_000_000,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let ast = parse("cpu[5m] offset 1h").expect("parse");
        evaluate(&ctx, &ast).expect("eval");
        let (start, end) = *ds.last_range.lock().unwrap();
        assert_eq!(end, 1_000_000 - 3_600_000);
        assert_eq!(start, 1_000_000 - 3_600_000 - 300_000);
    }

    #[test]
    fn negative_offset_shifts_anchor_forward() {
        let ds = RecordingDataSource {
            series: vec![series(&[("__name__", "cpu")], &[(0, 1.0)])],
            last_range: std::sync::Mutex::new((-1, -1)),
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 1_000_000,
            end_ms: 1_000_000,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let ast = parse("cpu offset -1h").expect("parse");
        evaluate(&ctx, &ast).expect("eval");
        let (_, end) = *ds.last_range.lock().unwrap();
        assert_eq!(end, 1_000_000 + 3_600_000);
    }

    #[test]
    fn at_modifier_overrides_anchor() {
        let ds = RecordingDataSource {
            series: vec![series(&[("__name__", "cpu")], &[(0, 1.0)])],
            last_range: std::sync::Mutex::new((-1, -1)),
        };
        // anchor at 12345 seconds → 12_345_000 ms.
        let ctx = EvalContext {
            data: &ds,
            start_ms: 1_000_000,
            end_ms: 1_000_000,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let ast = parse("cpu @ 12345").expect("parse");
        evaluate(&ctx, &ast).expect("eval");
        let (_, end) = *ds.last_range.lock().unwrap();
        assert_eq!(end, 12_345_000);
    }

    #[test]
    fn duration_units_compose() {
        // 1h30m45s = 5445 s = 5_445_000 ms.
        assert_eq!(parse_duration_ms("1h30m45s", 1).unwrap(), 5_445_000);
        // 0.5h = 30 min = 1_800_000 ms.
        assert_eq!(parse_duration_ms("0.5h", 1).unwrap(), 1_800_000);
        // Bare number → seconds.
        assert_eq!(parse_duration_ms("30", 1).unwrap(), 30_000);
        // Step-relative.
        assert_eq!(parse_duration_ms("1i", 250).unwrap(), 250);
    }

    #[test]
    fn subquery_evaluates_inner_at_substeps() {
        // `max_over_time(cpu[60s:10s])` — sub-evaluate cpu at
        // 7 sub-steps in the 60s window stepping every 10s,
        // then take the max. Inputs grow linearly so the max
        // is simply the latest observation in the window.
        let ds = MemoryDataSource {
            series: vec![series(
                &[("__name__", "cpu"), ("host", "a")],
                &[
                    (0, 0.0),
                    (10_000, 1.0),
                    (20_000, 2.0),
                    (30_000, 3.0),
                    (40_000, 4.0),
                    (50_000, 5.0),
                    (60_000, 6.0),
                ],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 60_000,
            end_ms: 60_000,
            step_ms: 1,
            lookback_ms: Some(15_000),
            query_start_ms: None,
            query_end_ms: None,
        };
        let got =
            evaluate(&ctx, &parse("max_over_time(cpu[60s:10s])").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].samples[0].value, 6.0);
    }

    #[test]
    fn subquery_inherit_step_borrows_outer_step() {
        // `expr[1m:]` — inherit the outer query step. With a
        // 30s outer step the subquery walks two sub-steps.
        let ds = MemoryDataSource {
            series: vec![series(
                &[("__name__", "cpu"), ("host", "a")],
                &[(0, 1.0), (30_000, 5.0), (60_000, 10.0)],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 60_000,
            end_ms: 60_000,
            step_ms: 30_000,
            lookback_ms: Some(35_000),
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate(&ctx, &parse("avg_over_time(cpu[1m:])").expect("parse")).expect("eval");
        assert!(got[0].samples[0].value > 0.0);
    }

    #[test]
    fn at_start_resolves_to_query_start() {
        // `@ start()` should pin to the outer query start.
        // Use WindowedDataSource so the inner [start, end]
        // window actually filters samples.
        let ds = WindowedDataSource {
            series: vec![series(
                &[("__name__", "cpu"), ("host", "a")],
                &[(1000, 100.0), (3000, 300.0), (5000, 500.0)],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 1000,
            end_ms: 5000,
            step_ms: 2000,
            lookback_ms: Some(10),
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate_range(&ctx, &parse("cpu @ start()").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        // Every step's `cpu @ start()` resolves to t=1000 →
        // value 100. Range merge collapses into one series
        // with multiple samples (one per step), all at 100.
        assert!(!got[0].samples.is_empty());
        for s in &got[0].samples {
            assert_eq!(s.value, 100.0);
        }
    }

    #[test]
    fn at_end_resolves_to_query_end() {
        let ds = WindowedDataSource {
            series: vec![series(
                &[("__name__", "cpu"), ("host", "a")],
                &[(1000, 100.0), (5000, 500.0)],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 1000,
            end_ms: 5000,
            step_ms: 2000,
            lookback_ms: Some(10),
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate_range(&ctx, &parse("cpu @ end()").expect("parse")).expect("eval");
        for s in &got[0].samples {
            assert_eq!(s.value, 500.0);
        }
    }

    #[test]
    fn at_modifier_rejects_unknown_function() {
        let ds = MemoryDataSource { series: vec![] };
        let ctx = ctx_for(&ds);
        let err = evaluate(&ctx, &parse("cpu @ now()").expect("parse")).expect_err("eval");
        assert!(matches!(err, EvalError::BadValue(_)));
    }

    fn lookup_label<'a>(s: &'a Series, key: &str) -> Option<&'a str> {
        s.labels
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn sum_collapses_all_into_one_series() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "cpu"), ("host", "a")], &[(0, 1.0)]),
                series(&[("__name__", "cpu"), ("host", "b")], &[(0, 2.0)]),
                series(&[("__name__", "cpu"), ("host", "c")], &[(0, 4.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        let ast = parse("sum(cpu)").expect("parse");
        let got = evaluate(&ctx, &ast).expect("eval");
        assert_eq!(got.len(), 1);
        assert!(got[0].labels.is_empty());
        assert_eq!(got[0].samples.len(), 1);
        assert_eq!(got[0].samples[0].value, 7.0);
    }

    #[test]
    fn sum_by_groups_on_named_labels() {
        let ds = MemoryDataSource {
            series: vec![
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
            ],
        };
        let ctx = ctx_for(&ds);
        let ast = parse("sum(cpu) by (zone)").expect("parse");
        let mut got = evaluate(&ctx, &ast).expect("eval");
        got.sort_by(|a, b| {
            lookup_label(a, "zone")
                .unwrap_or("")
                .cmp(lookup_label(b, "zone").unwrap_or(""))
        });
        assert_eq!(got.len(), 2);
        assert_eq!(lookup_label(&got[0], "zone"), Some("z1"));
        assert_eq!(got[0].samples[0].value, 3.0);
        assert_eq!(lookup_label(&got[1], "zone"), Some("z2"));
        assert_eq!(got[1].samples[0].value, 4.0);
    }

    #[test]
    fn sum_without_drops_listed_labels_and_metric_name() {
        let ds = MemoryDataSource {
            series: vec![
                series(
                    &[("__name__", "cpu"), ("zone", "z1"), ("host", "a")],
                    &[(0, 1.0)],
                ),
                series(
                    &[("__name__", "cpu"), ("zone", "z1"), ("host", "b")],
                    &[(0, 2.0)],
                ),
            ],
        };
        let ctx = ctx_for(&ds);
        // `without (host)` keeps `zone`; `__name__` is dropped
        // automatically per upstream aggregate semantics.
        let ast = parse("sum(cpu) without (host)").expect("parse");
        let got = evaluate(&ctx, &ast).expect("eval");
        assert_eq!(got.len(), 1);
        assert_eq!(lookup_label(&got[0], "zone"), Some("z1"));
        assert_eq!(lookup_label(&got[0], "__name__"), None);
        assert_eq!(lookup_label(&got[0], "host"), None);
        assert_eq!(got[0].samples[0].value, 3.0);
    }

    #[test]
    fn avg_min_max_count_group_share_grouping_logic() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "cpu"), ("host", "a")], &[(0, 1.0)]),
                series(&[("__name__", "cpu"), ("host", "b")], &[(0, 5.0)]),
                series(&[("__name__", "cpu"), ("host", "c")], &[(0, 9.0)]),
            ],
        };
        let ctx = ctx_for(&ds);

        let go = |q: &str| -> f64 {
            let ast = parse(q).expect("parse");
            let got = evaluate(&ctx, &ast).expect("eval");
            assert_eq!(got.len(), 1);
            got[0].samples[0].value
        };
        assert_eq!(go("avg(cpu)"), 5.0);
        assert_eq!(go("min(cpu)"), 1.0);
        assert_eq!(go("max(cpu)"), 9.0);
        assert_eq!(go("count(cpu)"), 3.0);
        assert_eq!(go("group(cpu)"), 1.0);
    }

    #[test]
    fn aggregate_skips_nan_inputs() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "cpu"), ("host", "a")], &[(0, 2.0)]),
                series(&[("__name__", "cpu"), ("host", "b")], &[(0, f64::NAN)]),
                series(&[("__name__", "cpu"), ("host", "c")], &[(0, 4.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        let ast = parse("count(cpu)").expect("parse");
        let got = evaluate(&ctx, &ast).expect("eval");
        // NaN sample doesn't contribute to count.
        assert_eq!(got[0].samples[0].value, 2.0);
    }

    #[test]
    fn aggregate_aligns_samples_per_timestamp() {
        let ds = MemoryDataSource {
            series: vec![
                series(
                    &[("__name__", "cpu"), ("host", "a")],
                    &[(10, 1.0), (20, 2.0)],
                ),
                series(
                    &[("__name__", "cpu"), ("host", "b")],
                    &[(10, 3.0), (20, 4.0)],
                ),
            ],
        };
        let ctx = ctx_for(&ds);
        let ast = parse("sum(cpu)").expect("parse");
        let got = evaluate(&ctx, &ast).expect("eval");
        assert_eq!(got[0].samples.len(), 2);
        assert_eq!(got[0].samples[0].timestamp_ms, 10);
        assert_eq!(got[0].samples[0].value, 4.0);
        assert_eq!(got[0].samples[1].timestamp_ms, 20);
        assert_eq!(got[0].samples[1].value, 6.0);
    }

    #[test]
    fn unknown_function_reports_not_yet_implemented() {
        let ds = MemoryDataSource { series: vec![] };
        let ctx = ctx_for(&ds);
        // `clamp_min` isn't a rollup or aggregate — should
        // surface NotYetImplemented, not panic.
        let ast = parse("clamp_min(cpu, 0)").expect("parse");
        let err = evaluate(&ctx, &ast).expect_err("eval");
        assert!(matches!(err, EvalError::NotYetImplemented(_)));
    }

    #[test]
    fn rate_computes_per_second_delta_over_window() {
        let ds = WindowedDataSource {
            series: vec![series(
                &[("__name__", "counter"), ("host", "a")],
                // Counter rises 100 over 60s → rate is 100/60.
                &[(0, 0.0), (30_000, 50.0), (60_000, 100.0)],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 60_000,
            end_ms: 60_000,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate(&ctx, &parse("rate(counter[60s])").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        assert_eq!(lookup_label(&got[0], "__name__"), None);
        assert_eq!(lookup_label(&got[0], "host"), Some("a"));
        assert!((got[0].samples[0].value - (100.0 / 60.0)).abs() < 1e-9);
    }

    #[test]
    fn increase_returns_last_minus_first() {
        let ds = WindowedDataSource {
            series: vec![series(
                &[("__name__", "c")],
                &[(0, 5.0), (1000, 8.0), (2000, 12.0)],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 2000,
            end_ms: 2000,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate(&ctx, &parse("increase(c[2s])").expect("parse")).expect("eval");
        assert_eq!(got[0].samples[0].value, 7.0);
    }

    #[test]
    fn delta_works_for_negative_change() {
        let ds = WindowedDataSource {
            series: vec![series(&[("__name__", "g")], &[(0, 100.0), (1000, 80.0)])],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 1000,
            end_ms: 1000,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate(&ctx, &parse("delta(g[1s])").expect("parse")).expect("eval");
        assert_eq!(got[0].samples[0].value, -20.0);
    }

    #[test]
    fn sum_over_time_adds_all_samples() {
        let ds = WindowedDataSource {
            series: vec![series(
                &[("__name__", "v")],
                &[(0, 1.0), (10, 2.0), (20, 3.0)],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 20,
            end_ms: 20,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate(&ctx, &parse("sum_over_time(v[20ms])").expect("parse")).expect("eval");
        assert_eq!(got[0].samples[0].value, 6.0);
    }

    #[test]
    fn over_time_family_share_window() {
        let ds = WindowedDataSource {
            series: vec![series(
                &[("__name__", "v")],
                &[(0, 1.0), (10, 7.0), (20, 4.0)],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 20,
            end_ms: 20,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let go = |q: &str| -> f64 {
            let got = evaluate(&ctx, &parse(q).expect("parse")).expect("eval");
            got[0].samples[0].value
        };
        assert_eq!(go("avg_over_time(v[20ms])"), 4.0);
        assert_eq!(go("min_over_time(v[20ms])"), 1.0);
        assert_eq!(go("max_over_time(v[20ms])"), 7.0);
        assert_eq!(go("count_over_time(v[20ms])"), 3.0);
        assert_eq!(go("first_over_time(v[20ms])"), 1.0);
        // t*_over_time report WHEN, not what: Unix seconds of the window's edge
        // samples (0ms and 20ms here), which is what makes elapsed-so-far
        // computable for a phase that has not finished.
        assert_eq!(go("tfirst_over_time(v[20ms])"), 0.0);
        assert_eq!(go("tlast_over_time(v[20ms])"), 0.020);
        assert_eq!(
            go("tlast_over_time(v[20ms])") - go("tfirst_over_time(v[20ms])"),
            0.020
        );
        // Values here move at every sample, so first/last change are the 10ms and
        // 20ms samples — the first sample cannot be a "change", having nothing before it.
        assert_eq!(go("tlast_change_over_time(v[20ms])"), 0.020);
        assert_eq!(go("last_over_time(v[20ms])"), 4.0);
    }

    /// A series that stops moving must report the moment it stopped, not the end
    /// of the recording. Metric instances keep being sampled at their last value
    /// long after the work feeding them ends, so "last sample" and "last change"
    /// are different questions and only the second one bounds activity.
    #[test]
    fn change_timestamps_ignore_repeated_values() {
        let ds = WindowedDataSource {
            series: vec![series(
                &[("__name__", "v")],
                &[(0, 1.0), (10, 5.0), (20, 5.0), (30, 5.0)],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 30,
            end_ms: 30,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let go = |q: &str| -> f64 {
            evaluate(&ctx, &parse(q).expect("parse")).expect("eval")[0].samples[0].value
        };
        assert_eq!(
            go("tlast_over_time(v[30ms])"),
            0.030,
            "last SAMPLE is the flat tail"
        );
        assert_eq!(
            go("tlast_change_over_time(v[30ms])"),
            0.010,
            "last CHANGE is when it stopped moving"
        );
        // Elapsed-while-active, which is what a still-running phase needs.
        assert_eq!(
            go("tlast_change_over_time(v[30ms])") - go("tfirst_over_time(v[30ms])"),
            0.010
        );
    }

    #[test]
    fn rollup_fn_emits_at_query_anchor_not_sample_times() {
        let ds = WindowedDataSource {
            series: vec![series(&[("__name__", "v")], &[(100, 5.0), (200, 9.0)])],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 200,
            end_ms: 200,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate(&ctx, &parse("max_over_time(v[200ms])").expect("parse")).expect("eval");
        assert_eq!(got[0].samples.len(), 1);
        assert_eq!(got[0].samples[0].timestamp_ms, 200);
    }

    #[test]
    fn rate_without_window_is_rejected() {
        let ds = WindowedDataSource {
            series: vec![series(&[("__name__", "v")], &[(0, 1.0)])],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 0,
            end_ms: 0,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        // No `[w]` window — current code requires it.
        let err = evaluate(&ctx, &parse("rate(v)").expect("parse")).expect_err("eval");
        assert!(matches!(err, EvalError::BadValue(_)));
    }

    /// Backend that always fails. Verifies the evaluator
    /// surfaces storage errors via `EvalError::DataSource`
    /// instead of swallowing or remapping them.
    struct FailingDataSource {
        message: &'static str,
    }

    impl MetricAccess for FailingDataSource {
        fn select_range(&self, _: &[Matcher], _: i64, _: i64) -> Result<Vector, DataSourceError> {
            Err(DataSourceError::new(self.message))
        }
    }

    #[test]
    fn data_source_error_propagates_as_evalerror_datasource() {
        let ds = FailingDataSource {
            message: "sqlite died",
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 0,
            end_ms: 0,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let err = evaluate(&ctx, &parse("cpu").expect("parse")).expect_err("eval");
        match err {
            EvalError::DataSource(e) => assert_eq!(e.message, "sqlite died"),
            other => panic!("expected DataSource error, got {other:?}"),
        }
    }

    #[test]
    fn rate_over_range_query_emits_per_step() {
        let ds = WindowedDataSource {
            series: vec![series(
                &[("__name__", "c"), ("host", "h1")],
                &[(0, 0.0), (1000, 1.0), (2000, 2.0), (3000, 3.0)],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 1000,
            end_ms: 3000,
            step_ms: 1000,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate_range(&ctx, &parse("rate(c[1s])").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        // Per-second rate is 1.0 across the steady-rate counter.
        for sm in &got[0].samples {
            assert!((sm.value - 1.0).abs() < 1e-9);
        }
        let timestamps: Vec<i64> = got[0].samples.iter().map(|s| s.timestamp_ms).collect();
        assert_eq!(timestamps, vec![1000, 2000, 3000]);
    }

    #[test]
    fn scalar_op_scalar_produces_label_less_series() {
        let ds = MemoryDataSource { series: vec![] };
        let ctx = ctx_for(&ds);
        // Constant fold collapses `2 + 3` at parse time, so
        // wrap one side in a non-foldable construct.
        let ast = parse("2 + 3").expect("parse");
        let got = evaluate(&ctx, &ast).expect("eval");
        // Folded already, but the eval path still has to
        // accept the resulting Number.
        assert_eq!(got.len(), 1);
        assert!(got[0].labels.is_empty());
        assert_eq!(got[0].samples[0].value, 5.0);
    }

    #[test]
    fn vector_plus_scalar_broadcasts() {
        let ds = MemoryDataSource {
            series: vec![
                series(
                    &[("__name__", "cpu"), ("host", "a")],
                    &[(0, 1.0), (10, 2.0)],
                ),
                series(
                    &[("__name__", "cpu"), ("host", "b")],
                    &[(0, 3.0), (10, 4.0)],
                ),
            ],
        };
        let ctx = ctx_for(&ds);
        let ast = parse("cpu + 10").expect("parse");
        let mut got = evaluate(&ctx, &ast).expect("eval");
        got.sort_by(|a, b| {
            lookup_label(a, "host")
                .unwrap_or("")
                .cmp(lookup_label(b, "host").unwrap_or(""))
        });
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].samples[0].value, 11.0);
        assert_eq!(got[0].samples[1].value, 12.0);
        // Result drops `__name__` per upstream binary-op rules.
        assert_eq!(lookup_label(&got[0], "__name__"), None);
        assert_eq!(lookup_label(&got[0], "host"), Some("a"));
    }

    #[test]
    fn scalar_minus_vector_keeps_operand_order() {
        let ds = MemoryDataSource {
            series: vec![series(&[("__name__", "cpu"), ("host", "a")], &[(0, 4.0)])],
        };
        let ctx = ctx_for(&ds);
        // 10 - 4 = 6, NOT 4 - 10 = -6.
        let ast = parse("10 - cpu").expect("parse");
        let got = evaluate(&ctx, &ast).expect("eval");
        assert_eq!(got[0].samples[0].value, 6.0);
    }

    #[test]
    fn vector_times_vector_matches_on_labels_drops_metric_name() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "a"), ("host", "h1")], &[(0, 2.0)]),
                series(&[("__name__", "a"), ("host", "h2")], &[(0, 5.0)]),
                series(&[("__name__", "b"), ("host", "h1")], &[(0, 3.0)]),
                series(&[("__name__", "b"), ("host", "h2")], &[(0, 7.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        let ast = parse("a * b").expect("parse");
        let mut got = evaluate(&ctx, &ast).expect("eval");
        got.sort_by(|x, y| {
            lookup_label(x, "host")
                .unwrap_or("")
                .cmp(lookup_label(y, "host").unwrap_or(""))
        });
        assert_eq!(got.len(), 2);
        assert_eq!(lookup_label(&got[0], "__name__"), None);
        assert_eq!(lookup_label(&got[0], "host"), Some("h1"));
        assert_eq!(got[0].samples[0].value, 6.0);
        assert_eq!(lookup_label(&got[1], "host"), Some("h2"));
        assert_eq!(got[1].samples[0].value, 35.0);
    }

    #[test]
    fn vector_op_vector_unmatched_labels_drop_out() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "a"), ("host", "h1")], &[(0, 2.0)]),
                series(&[("__name__", "b"), ("host", "h2")], &[(0, 5.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        let ast = parse("a + b").expect("parse");
        let got = evaluate(&ctx, &ast).expect("eval");
        // No label-set overlap between the two series → empty.
        assert!(got.is_empty());
    }

    #[test]
    fn timestamp_alignment_inner_joins_samples() {
        let ds = MemoryDataSource {
            series: vec![
                series(
                    &[("__name__", "a"), ("host", "h1")],
                    &[(10, 1.0), (20, 2.0), (30, 3.0)],
                ),
                series(
                    &[("__name__", "b"), ("host", "h1")],
                    &[(20, 4.0), (30, 5.0), (40, 6.0)],
                ),
            ],
        };
        let ctx = ctx_for(&ds);
        let ast = parse("a + b").expect("parse");
        let got = evaluate(&ctx, &ast).expect("eval");
        assert_eq!(got.len(), 1);
        // Only timestamps 20 and 30 appear in both sides.
        assert_eq!(got[0].samples.len(), 2);
        assert_eq!(got[0].samples[0].timestamp_ms, 20);
        assert_eq!(got[0].samples[0].value, 6.0);
        assert_eq!(got[0].samples[1].timestamp_ms, 30);
        assert_eq!(got[0].samples[1].value, 8.0);
    }

    #[test]
    fn comparison_with_bool_produces_zero_or_one() {
        let ds = MemoryDataSource {
            series: vec![series(&[("__name__", "cpu"), ("host", "a")], &[(0, 4.0)])],
        };
        let ctx = ctx_for(&ds);
        let ast = parse("cpu >bool 3").expect("parse");
        let got = evaluate(&ctx, &ast).expect("eval");
        assert_eq!(got[0].samples[0].value, 1.0);

        let ast = parse("cpu >bool 5").expect("parse");
        let got = evaluate(&ctx, &ast).expect("eval");
        assert_eq!(got[0].samples[0].value, 0.0);
    }

    #[test]
    fn filter_mode_comparison_drops_unmatched_samples() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "cpu"), ("host", "a")], &[(0, 4.0)]),
                series(&[("__name__", "cpu"), ("host", "b")], &[(0, 2.0)]),
                series(&[("__name__", "cpu"), ("host", "c")], &[(0, 5.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        // `cpu > 3` keeps only the samples (and series) whose
        // value beats 3; `host=b` drops out entirely.
        let ast = parse("cpu > 3").expect("parse");
        let mut got = evaluate(&ctx, &ast).expect("eval");
        got.sort_by(|a, b| {
            lookup_label(a, "host")
                .unwrap_or("")
                .cmp(lookup_label(b, "host").unwrap_or(""))
        });
        assert_eq!(got.len(), 2);
        assert_eq!(lookup_label(&got[0], "host"), Some("a"));
        assert_eq!(got[0].samples[0].value, 4.0);
        assert_eq!(lookup_label(&got[1], "host"), Some("c"));
        assert_eq!(got[1].samples[0].value, 5.0);
    }

    #[test]
    fn filter_mode_drops_series_left_with_no_samples() {
        let ds = MemoryDataSource {
            series: vec![series(
                &[("__name__", "cpu"), ("host", "a")],
                &[(0, 1.0), (10, 2.0)],
            )],
        };
        let ctx = ctx_for(&ds);
        let ast = parse("cpu > 100").expect("parse");
        let got = evaluate(&ctx, &ast).expect("eval");
        // Every sample fails the predicate → empty result.
        assert!(got.is_empty());
    }

    #[test]
    fn and_keeps_left_samples_present_on_right() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "a"), ("host", "h1")], &[(0, 1.0)]),
                series(&[("__name__", "a"), ("host", "h2")], &[(0, 2.0)]),
                series(&[("__name__", "b"), ("host", "h1")], &[(0, 9.0)]),
                // No `b` series for `h2`.
            ],
        };
        let ctx = ctx_for(&ds);
        let got = evaluate(&ctx, &parse("a and b").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        assert_eq!(lookup_label(&got[0], "host"), Some("h1"));
        // `and` keeps left's value, not right's.
        assert_eq!(got[0].samples[0].value, 1.0);
    }

    #[test]
    fn or_unions_disjoint_series() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "a"), ("host", "h1")], &[(0, 1.0)]),
                series(&[("__name__", "b"), ("host", "h2")], &[(0, 2.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        let got = evaluate(&ctx, &parse("a or b").expect("parse")).expect("eval");
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn or_prefers_left_when_both_match() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "a"), ("host", "h1")], &[(0, 1.0)]),
                series(&[("__name__", "b"), ("host", "h1")], &[(0, 99.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        let got = evaluate(&ctx, &parse("a or b").expect("parse")).expect("eval");
        // Only the `a` series survives — `b` was suppressed
        // because the same (labels-without-name, ts) was on
        // the left.
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].samples[0].value, 1.0);
    }

    #[test]
    fn unless_drops_left_samples_present_on_right() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "a"), ("host", "h1")], &[(0, 1.0)]),
                series(&[("__name__", "a"), ("host", "h2")], &[(0, 2.0)]),
                series(&[("__name__", "b"), ("host", "h1")], &[(0, 99.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        let got = evaluate(&ctx, &parse("a unless b").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        assert_eq!(lookup_label(&got[0], "host"), Some("h2"));
    }

    #[test]
    fn default_replaces_nan_left_with_right() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "a"), ("host", "h1")], &[(0, f64::NAN)]),
                series(&[("__name__", "b"), ("host", "h1")], &[(0, 5.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        let got = evaluate(&ctx, &parse("a default b").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].samples[0].value, 5.0);
    }

    #[test]
    fn if_keeps_left_when_right_is_present() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "a"), ("host", "h1")], &[(0, 7.0)]),
                series(&[("__name__", "b"), ("host", "h1")], &[(0, 1.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        let got = evaluate(&ctx, &parse("a if b").expect("parse")).expect("eval");
        assert_eq!(got[0].samples[0].value, 7.0);
    }

    #[test]
    fn ifnot_keeps_left_when_right_is_nan() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "a"), ("host", "h1")], &[(0, 7.0)]),
                series(&[("__name__", "b"), ("host", "h1")], &[(0, f64::NAN)]),
            ],
        };
        let ctx = ctx_for(&ds);
        let got = evaluate(&ctx, &parse("a ifnot b").expect("parse")).expect("eval");
        assert_eq!(got[0].samples[0].value, 7.0);
    }

    #[test]
    fn on_modifier_restricts_match_to_listed_labels() {
        let ds = MemoryDataSource {
            series: vec![
                series(
                    &[("__name__", "a"), ("host", "h1"), ("zone", "z1")],
                    &[(0, 2.0)],
                ),
                // Right side has same `host` but different
                // `zone`; the default key would not match,
                // but `on(host)` ignores `zone`.
                series(
                    &[("__name__", "b"), ("host", "h1"), ("zone", "z2")],
                    &[(0, 5.0)],
                ),
            ],
        };
        let ctx = ctx_for(&ds);
        let got = evaluate(&ctx, &parse("a + on(host) b").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].samples[0].value, 7.0);
        // Result labels = the matching key only.
        assert_eq!(lookup_label(&got[0], "host"), Some("h1"));
        assert_eq!(lookup_label(&got[0], "zone"), None);
    }

    #[test]
    fn ignoring_modifier_drops_listed_labels_from_match_key() {
        let ds = MemoryDataSource {
            series: vec![
                series(
                    &[("__name__", "a"), ("host", "h1"), ("zone", "z1")],
                    &[(0, 2.0)],
                ),
                series(
                    &[("__name__", "b"), ("host", "h1"), ("zone", "z2")],
                    &[(0, 5.0)],
                ),
            ],
        };
        let ctx = ctx_for(&ds);
        let got = evaluate(&ctx, &parse("a + ignoring(zone) b").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].samples[0].value, 7.0);
        // `ignoring(zone)` keeps `host`; `__name__` always
        // dropped.
        assert_eq!(lookup_label(&got[0], "host"), Some("h1"));
        assert_eq!(lookup_label(&got[0], "zone"), None);
    }

    #[test]
    fn group_left_carries_extra_labels_from_right() {
        let ds = MemoryDataSource {
            series: vec![
                series(
                    &[("__name__", "a"), ("host", "h1"), ("region", "r1")],
                    &[(0, 10.0)],
                ),
                series(
                    &[("__name__", "a"), ("host", "h2"), ("region", "r1")],
                    &[(0, 20.0)],
                ),
                // The "one" side: keyed on region, carrying
                // an extra `tier` label group_left will copy.
                series(
                    &[("__name__", "b"), ("region", "r1"), ("tier", "prod")],
                    &[(0, 2.0)],
                ),
            ],
        };
        let ctx = ctx_for(&ds);
        let got = evaluate(
            &ctx,
            &parse("a * on(region) group_left(tier) b").expect("parse"),
        )
        .expect("eval");
        assert_eq!(got.len(), 2);
        for s in &got {
            assert_eq!(
                s.samples[0].value,
                match lookup_label(s, "host") {
                    Some("h1") => 20.0,
                    Some("h2") => 40.0,
                    _ => panic!("unexpected host"),
                }
            );
            assert_eq!(lookup_label(s, "region"), Some("r1"));
            // `tier` was copied across from the right.
            assert_eq!(lookup_label(s, "tier"), Some("prod"));
        }
    }

    #[test]
    fn group_right_is_symmetric_to_group_left() {
        let ds = MemoryDataSource {
            series: vec![
                series(
                    &[("__name__", "a"), ("region", "r1"), ("tier", "prod")],
                    &[(0, 2.0)],
                ),
                series(
                    &[("__name__", "b"), ("host", "h1"), ("region", "r1")],
                    &[(0, 10.0)],
                ),
                series(
                    &[("__name__", "b"), ("host", "h2"), ("region", "r1")],
                    &[(0, 20.0)],
                ),
            ],
        };
        let ctx = ctx_for(&ds);
        let got = evaluate(
            &ctx,
            &parse("a * on(region) group_right(tier) b").expect("parse"),
        )
        .expect("eval");
        assert_eq!(got.len(), 2);
        for s in &got {
            assert_eq!(lookup_label(s, "tier"), Some("prod"));
            // The "many" side is the rhs (b), so `host`
            // labels survive on each result.
            assert!(lookup_label(s, "host").is_some());
        }
    }

    #[test]
    fn on_modifier_works_with_set_ops_too() {
        let ds = MemoryDataSource {
            series: vec![
                series(
                    &[("__name__", "a"), ("host", "h1"), ("zone", "z1")],
                    &[(0, 1.0)],
                ),
                series(
                    &[("__name__", "a"), ("host", "h2"), ("zone", "z1")],
                    &[(0, 2.0)],
                ),
                // Right side keys on `host`; only h1 has a
                // counterpart.
                series(
                    &[("__name__", "b"), ("host", "h1"), ("zone", "z9")],
                    &[(0, 9.0)],
                ),
            ],
        };
        let ctx = ctx_for(&ds);
        let got = evaluate(&ctx, &parse("a and on(host) b").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        assert_eq!(lookup_label(&got[0], "host"), Some("h1"));
    }

    #[test]
    fn group_left_on_set_op_is_rejected() {
        let ds = MemoryDataSource { series: vec![] };
        let ctx = ctx_for(&ds);
        let err = evaluate(&ctx, &parse("a and on(h) group_left(x) b").expect("parse"))
            .expect_err("eval");
        assert!(matches!(err, EvalError::BadValue(_)));
    }

    /// Time-aware in-memory source: returns each series's
    /// samples filtered to the requested `[start, end]`. Lets
    /// range-query tests see what "the value at time T" means
    /// when T sweeps across the query window.
    struct WindowedDataSource {
        series: Vec<Series>,
    }

    impl MetricAccess for WindowedDataSource {
        fn select_range(
            &self,
            matchers: &[Matcher],
            start: i64,
            end: i64,
        ) -> Result<Vector, DataSourceError> {
            Ok(self
                .series
                .iter()
                .filter(|s| matchers.iter().all(|m| matches_series(m, s)))
                .map(|s| Series {
                    labels: s.labels.clone(),
                    samples: s
                        .samples
                        .iter()
                        .filter(|sm| sm.timestamp_ms >= start && sm.timestamp_ms <= end)
                        .cloned()
                        .collect(),
                })
                .collect())
        }
    }

    #[test]
    fn range_query_steps_through_window_and_merges_per_series() {
        let ds = WindowedDataSource {
            series: vec![series(
                &[("__name__", "cpu"), ("host", "a")],
                &[(0, 10.0), (10, 11.0), (20, 12.0), (30, 13.0)],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 0,
            end_ms: 30,
            step_ms: 10,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        // Each step returns the [T, T] sample for the host=a series.
        let got = evaluate_range(&ctx, &parse("cpu").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        let ts: Vec<_> = got[0]
            .samples
            .iter()
            .map(|s| (s.timestamp_ms, s.value))
            .collect();
        assert_eq!(ts, vec![(0, 10.0), (10, 11.0), (20, 12.0), (30, 13.0)]);
    }

    #[test]
    fn range_query_aggregate_evaluates_per_step() {
        let ds = WindowedDataSource {
            series: vec![
                series(
                    &[("__name__", "cpu"), ("host", "a")],
                    &[(0, 1.0), (10, 2.0)],
                ),
                series(
                    &[("__name__", "cpu"), ("host", "b")],
                    &[(0, 3.0), (10, 4.0)],
                ),
            ],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 0,
            end_ms: 10,
            step_ms: 10,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate_range(&ctx, &parse("sum(cpu)").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        let ts: Vec<_> = got[0]
            .samples
            .iter()
            .map(|s| (s.timestamp_ms, s.value))
            .collect();
        // sum at T=0: 1+3=4. sum at T=10: 2+4=6.
        assert_eq!(ts, vec![(0, 4.0), (10, 6.0)]);
    }

    #[test]
    fn range_query_step_zero_is_rejected() {
        let ds = WindowedDataSource { series: vec![] };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 0,
            end_ms: 10,
            step_ms: 0,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let err = evaluate_range(&ctx, &parse("cpu").expect("parse")).expect_err("eval");
        assert!(matches!(err, EvalError::BadValue(_)));
    }

    #[test]
    fn range_query_clamps_final_step_to_end() {
        // Step of 7 over [0, 20] should hit T=0, 7, 14, 20 —
        // the last step lands on the boundary even though
        // it's a partial step from the previous one.
        let ds = WindowedDataSource {
            series: vec![series(
                &[("__name__", "cpu")],
                &[(0, 1.0), (7, 2.0), (14, 3.0), (20, 4.0)],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 0,
            end_ms: 20,
            step_ms: 7,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate_range(&ctx, &parse("cpu").expect("parse")).expect("eval");
        let timestamps: Vec<i64> = got[0].samples.iter().map(|s| s.timestamp_ms).collect();
        assert_eq!(timestamps, vec![0, 7, 14, 20]);
    }

    #[test]
    fn range_query_instant_window_yields_one_step() {
        let ds = WindowedDataSource {
            series: vec![series(&[("__name__", "cpu")], &[(5, 42.0)])],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 5,
            end_ms: 5,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate_range(&ctx, &parse("cpu").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].samples.len(), 1);
        assert_eq!(got[0].samples[0].value, 42.0);
    }

    #[test]
    fn aggregate_then_arithmetic_composes() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "cpu"), ("host", "a")], &[(0, 1.0)]),
                series(&[("__name__", "cpu"), ("host", "b")], &[(0, 2.0)]),
                series(&[("__name__", "cpu"), ("host", "c")], &[(0, 3.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        let ast = parse("sum(cpu) * 2").expect("parse");
        let got = evaluate(&ctx, &ast).expect("eval");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].samples[0].value, 12.0);
    }

    #[test]
    fn topk_picks_largest_per_timestamp_preserving_labels() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "cpu"), ("host", "a")], &[(0, 1.0), (1, 9.0)]),
                series(&[("__name__", "cpu"), ("host", "b")], &[(0, 5.0), (1, 2.0)]),
                series(&[("__name__", "cpu"), ("host", "c")], &[(0, 3.0), (1, 7.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        let got = evaluate(&ctx, &parse("topk(2, cpu)").expect("parse")).expect("eval");
        // 3 series total, but each timestamp picks 2; series may
        // appear in output if they made the cut at any timestamp.
        // At t=0: b=5, c=3 win. At t=1: a=9, c=7 win.
        // So a, b, c all appear, but with different ts subsets.
        let by_host = |h: &str| {
            got.iter()
                .find(|s| s.labels.iter().any(|(k, v)| k == "host" && v == h))
                .cloned()
        };
        let a = by_host("a").expect("host=a");
        let b = by_host("b").expect("host=b");
        let c = by_host("c").expect("host=c");
        assert_eq!(
            a.samples.iter().map(|s| s.timestamp_ms).collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(
            b.samples.iter().map(|s| s.timestamp_ms).collect::<Vec<_>>(),
            vec![0]
        );
        assert_eq!(
            c.samples.iter().map(|s| s.timestamp_ms).collect::<Vec<_>>(),
            vec![0, 1]
        );
        // Labels preserved (incl. __name__).
        assert!(a.labels.iter().any(|(k, v)| k == "__name__" && v == "cpu"));
    }

    #[test]
    fn bottomk_picks_smallest() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "cpu"), ("host", "a")], &[(0, 1.0)]),
                series(&[("__name__", "cpu"), ("host", "b")], &[(0, 5.0)]),
                series(&[("__name__", "cpu"), ("host", "c")], &[(0, 3.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        let got = evaluate(&ctx, &parse("bottomk(1, cpu)").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].samples[0].value, 1.0);
        assert!(got[0].labels.iter().any(|(k, v)| k == "host" && v == "a"));
    }

    #[test]
    fn quantile_collapses_to_phi_per_timestamp() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "cpu"), ("host", "a")], &[(0, 1.0)]),
                series(&[("__name__", "cpu"), ("host", "b")], &[(0, 2.0)]),
                series(&[("__name__", "cpu"), ("host", "c")], &[(0, 3.0)]),
                series(&[("__name__", "cpu"), ("host", "d")], &[(0, 4.0)]),
                series(&[("__name__", "cpu"), ("host", "e")], &[(0, 5.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        // phi=0.5 → median (linear interp on 5 values, sorted
        // [1,2,3,4,5], rank = 0.5*4 = 2.0 → exact 3.0).
        let got = evaluate(&ctx, &parse("quantile(0.5, cpu)").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].samples[0].value, 3.0);
        // phi=1 → max.
        let got = evaluate(&ctx, &parse("quantile(1.0, cpu)").expect("parse")).expect("eval");
        assert_eq!(got[0].samples[0].value, 5.0);
        // phi=0 → min.
        let got = evaluate(&ctx, &parse("quantile(0.0, cpu)").expect("parse")).expect("eval");
        assert_eq!(got[0].samples[0].value, 1.0);
    }

    #[test]
    fn quantile_linear_interpolates_between_ranks() {
        let ds = MemoryDataSource {
            series: vec![
                series(&[("__name__", "cpu"), ("host", "a")], &[(0, 0.0)]),
                series(&[("__name__", "cpu"), ("host", "b")], &[(0, 10.0)]),
            ],
        };
        let ctx = ctx_for(&ds);
        // Two values → phi=0.25 → rank 0.25 → 0 + 0.25*(10-0) = 2.5.
        let got = evaluate(&ctx, &parse("quantile(0.25, cpu)").expect("parse")).expect("eval");
        assert_eq!(got[0].samples[0].value, 2.5);
    }

    #[test]
    fn quantile_with_by_modifier_groups() {
        let ds = MemoryDataSource {
            series: vec![
                series(
                    &[("__name__", "cpu"), ("zone", "x"), ("host", "a")],
                    &[(0, 1.0)],
                ),
                series(
                    &[("__name__", "cpu"), ("zone", "x"), ("host", "b")],
                    &[(0, 3.0)],
                ),
                series(
                    &[("__name__", "cpu"), ("zone", "y"), ("host", "c")],
                    &[(0, 10.0)],
                ),
                series(
                    &[("__name__", "cpu"), ("zone", "y"), ("host", "d")],
                    &[(0, 20.0)],
                ),
            ],
        };
        let ctx = ctx_for(&ds);
        let got =
            evaluate(&ctx, &parse("quantile(0.5, cpu) by (zone)").expect("parse")).expect("eval");
        assert_eq!(got.len(), 2);
        let zone_x = got
            .iter()
            .find(|s| s.labels.iter().any(|(k, v)| k == "zone" && v == "x"))
            .expect("zone x");
        let zone_y = got
            .iter()
            .find(|s| s.labels.iter().any(|(k, v)| k == "zone" && v == "y"))
            .expect("zone y");
        assert_eq!(zone_x.samples[0].value, 2.0);
        assert_eq!(zone_y.samples[0].value, 15.0);
    }

    #[test]
    fn quantile_rejects_phi_out_of_range() {
        let ds = MemoryDataSource { series: vec![] };
        let ctx = ctx_for(&ds);
        let err = evaluate(&ctx, &parse("quantile(1.5, cpu)").expect("parse")).unwrap_err();
        assert!(matches!(err, EvalError::BadValue(_)));
    }

    #[test]
    fn rate_applies_counter_reset_adjustment() {
        // Counter wraps at sample 2 (10 → 1). True increase
        // across the window is (5 - 0) + 10 = 15 (the 10
        // pre-reset value gets added back when reset
        // is detected).
        // Window = [0, 60_000ms] = 60s; samples at 0, 30s, 60s
        // (so they sit at both edges and no extrapolation
        // factor kicks in).
        let ds = MemoryDataSource {
            series: vec![series(
                &[("__name__", "ctr"), ("host", "a")],
                &[(0, 0.0), (30_000, 10.0), (60_000, 5.0)],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 60_000,
            end_ms: 60_000,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate(&ctx, &parse("increase(ctr[60s])").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1);
        assert!(
            (got[0].samples[0].value - 15.0).abs() < 1e-9,
            "expected ~15, got {}",
            got[0].samples[0].value
        );
    }

    #[test]
    fn rate_divides_by_window_seconds() {
        // Linear counter 0→60 across 60s window: rate = 1/s.
        let ds = MemoryDataSource {
            series: vec![series(
                &[("__name__", "ctr"), ("host", "a")],
                &[(0, 0.0), (30_000, 30.0), (60_000, 60.0)],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 60_000,
            end_ms: 60_000,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate(&ctx, &parse("rate(ctr[60s])").expect("parse")).expect("eval");
        assert!(
            (got[0].samples[0].value - 1.0).abs() < 1e-9,
            "expected 1.0/s, got {}",
            got[0].samples[0].value
        );
    }

    #[test]
    fn rate_over_partial_window_is_observed_increase_no_extrapolation() {
        // Samples cover only the back half of the window `[0, 60s]`: the counter
        // is first seen at 30s (value 30) and reaches 60 at 60s. Bracket-rate is
        // cumulative-native (VM-aligned) — it does NOT extrapolate to fabricate
        // the unseen first half. With no sample at/before the window start it
        // takes the earliest sample as the left edge, so the increase is the
        // observed `60 − 30 = 30` over the full 60s window → 0.5/s. (Prometheus
        // would extrapolate to ~0.667/s; we deliberately report only what was
        // measured.)
        let ds = MemoryDataSource {
            series: vec![series(
                &[("__name__", "ctr"), ("host", "a")],
                &[
                    (30_000, 30.0),
                    (40_000, 40.0),
                    (50_000, 50.0),
                    (60_000, 60.0),
                ],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 60_000,
            end_ms: 60_000,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate(&ctx, &parse("rate(ctr[60s])").expect("parse")).expect("eval");
        let v = got[0].samples[0].value;
        assert!(
            (v - 0.5).abs() < 1e-9,
            "expected observed-increase/window = 0.5/s, got {v}"
        );
    }

    #[test]
    fn rate_brackets_pre_window_sample_when_window_narrower_than_cadence() {
        // The servo case: a `[400ms]` window against a 1s sample cadence catches
        // <2 samples strictly inside, which under the old `xs.len() < 2 → NaN`
        // rule produced no value. The bracketing sample at/before the window
        // start anchors the left edge, so rate is stable. Counter 100 at T−1s,
        // 140 at T. Window `[T−400ms, T]` → increase 40 over 0.4s → 100/s.
        let t = 10_000_000;
        let ds = MemoryDataSource {
            series: vec![series(
                &[("__name__", "errs")],
                &[(t - 1000, 100.0), (t, 140.0)],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: t,
            end_ms: t,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate(&ctx, &parse("rate(errs[400ms])").expect("parse")).expect("eval");
        assert_eq!(got.len(), 1, "rate is defined despite <2 in-window samples");
        let v = got[0].samples[0].value;
        assert!(
            (v - 100.0).abs() < 1e-9,
            "expected bracketed 100/s, got {v}"
        );
    }

    #[test]
    fn rate_of_flat_counter_is_zero_not_nan() {
        // The conc=2 servo case: the counter doesn't advance over the window.
        // The bracketing edges resolve to the same value → 0/s, a real and
        // stable reading, never NaN.
        let t = 10_000_000;
        let ds = MemoryDataSource {
            series: vec![series(
                &[("__name__", "errs")],
                &[(t - 1000, 7.0), (t, 7.0)],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: t,
            end_ms: t,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate(&ctx, &parse("rate(errs[400ms])").expect("parse")).expect("eval");
        let v = got[0].samples[0].value;
        assert!((v - 0.0).abs() < 1e-9, "flat counter → 0/s, got {v}");
    }

    #[test]
    fn delta_does_not_apply_counter_reset() {
        // For gauges (delta), a "negative jump" is real signal
        // — no compensation. (5 - 0) - (10 - 5) wraparound is
        // NOT detected; result is just last - first = -5.
        let ds = MemoryDataSource {
            series: vec![series(
                &[("__name__", "g"), ("host", "a")],
                &[(0, 0.0), (30_000, 10.0), (60_000, -5.0)],
            )],
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 60_000,
            end_ms: 60_000,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let got = evaluate(&ctx, &parse("delta(g[60s])").expect("parse")).expect("eval");
        assert_eq!(got[0].samples[0].value, -5.0);
    }

    #[test]
    fn topk_zero_returns_empty() {
        let ds = MemoryDataSource {
            series: vec![series(&[("__name__", "cpu"), ("host", "a")], &[(0, 1.0)])],
        };
        let ctx = ctx_for(&ds);
        let got = evaluate(&ctx, &parse("topk(0, cpu)").expect("parse")).expect("eval");
        assert!(got.is_empty());
    }
}
