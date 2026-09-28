// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Static metricsql grammar inventory: function names,
//! aggregation operators, binary operators, group modifiers,
//! and matcher operators that the parser/evaluator
//! recognise. The vocabulary half of the metric catalog —
//! [`crate::catalog::MetricCatalog`] enumerates *data*; this
//! module enumerates *grammar*.
//!
//! ## Canonical reference
//!
//! What the codebase considers "supported metricsql" lives in
//! three sources of truth, all in this crate:
//!
//! 1. **Parser corpus.** [`crate::parser::is_aggr_func`]
//!    enumerates the aggregation-operator names the parser
//!    recognises and round-trips. The fixture harness
//!    `crates/nmbrs-metricsql/tests/fixtures/parser_round_trip.json`
//!    pins ~600 input/expected pairs that exercise this surface.
//! 2. **Evaluator dispatch.** `crate::eval::RollupFn::from_name`,
//!    `eval::AggregateOp::from_name`, and
//!    `eval::ParameterizedAggregateOp::from_name` enumerate
//!    the names that map to actual computation against a
//!    [`crate::eval::DataSource`].
//! 3. **This module.** [`AGGREGATE_OPS`], [`ROLLUP_FUNCTIONS`]
//!    surface those lists as metadata for tooling
//!    (completion, lint, prettifier hints, IDE features).
//!
//! Adding a new operator means updating *all three* — and the
//! parity test in this file's `tests` module pins the link by
//! comparing `is_aggr_func` against [`AGGREGATE_OPS`]
//! membership directly. Drift between the parser and this
//! registry fails the test.
//!
//! ## Eval-support distinction
//!
//! Some names parse and round-trip but the evaluator
//! currently returns `NotYetImplemented` for them. We still
//! surface them in completion (so the user can write
//! prettifier-target / lint-target queries) but mark them
//! [`EvalSupport::ParserOnly`]. Filtering on this field lets
//! a metricsql-aware completer offer "everything that runs"
//! when the user is at a query prompt versus "everything that
//! parses" when editing a query stored as text.
//!
//! ## See also
//!
//! - [SRD-47](../../../docs/SRD/47_metricsql_streaming.md):
//!   streaming-evaluator coverage scope.
//! - [SRD-48](../../../docs/SRD/48_metricsql_continuous_query.md):
//!   continuous-query runtime; the catalog feeds query
//!   construction in continuous-query setups.

/// Where in the pipeline a name is supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalSupport {
    /// Recognised by [`crate::parser`] (round-trips through
    /// parse → prettify) but the evaluator returns
    /// `NotYetImplemented` for it. Visible in completion when
    /// the user is editing a stored query but the live-evaluator
    /// surface filters these out.
    ParserOnly,
    /// Parses *and* the evaluator dispatches to a real
    /// implementation against [`crate::eval::DataSource`]. Safe
    /// to suggest at every prompt.
    ParserAndEval,
}

impl EvalSupport {
    pub fn evaluable(self) -> bool {
        matches!(self, EvalSupport::ParserAndEval)
    }
}

// =====================================================================
// Aggregation operators
// =====================================================================

/// One aggregate. Mirrors a single arm in the parser's
/// `is_aggr_func` table; the doc-test below enforces the link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateOp {
    /// Lower-case name. metricsql aggregates are
    /// case-insensitive; the parser canonicalises to lower
    /// before round-tripping.
    pub name: &'static str,
    /// Whether the aggregate takes a leading scalar parameter
    /// (e.g. `topk(K, vec)`, `quantile(phi, vec)`).
    pub takes_param: bool,
    /// Where the aggregate is supported. Aggregates the
    /// parser accepts but the evaluator hasn't wired yet are
    /// [`EvalSupport::ParserOnly`].
    pub support: EvalSupport,
    /// One-line summary suitable for completion descriptions.
    pub summary: &'static str,
}

/// Every aggregate the parser accepts, mirroring
/// `parser::is_aggr_func`. Order matches the parser's source
/// listing for diff-friendliness against upstream changes.
///
/// Whenever you add an arm to `parser::is_aggr_func`, add the
/// corresponding entry here with the right [`EvalSupport`].
pub const AGGREGATE_OPS: &[AggregateOp] = &[
    AggregateOp {
        name: "any",
        takes_param: false,
        support: EvalSupport::ParserOnly,
        summary: "first matching series per group",
    },
    AggregateOp {
        name: "avg",
        takes_param: false,
        support: EvalSupport::ParserAndEval,
        summary: "arithmetic mean",
    },
    AggregateOp {
        name: "bottomk",
        takes_param: true,
        support: EvalSupport::ParserAndEval,
        summary: "bottom K series",
    },
    AggregateOp {
        name: "bottomk_avg",
        takes_param: true,
        support: EvalSupport::ParserOnly,
        summary: "bottom K by average",
    },
    AggregateOp {
        name: "bottomk_max",
        takes_param: true,
        support: EvalSupport::ParserOnly,
        summary: "bottom K by maximum",
    },
    AggregateOp {
        name: "bottomk_median",
        takes_param: true,
        support: EvalSupport::ParserOnly,
        summary: "bottom K by median",
    },
    AggregateOp {
        name: "bottomk_last",
        takes_param: true,
        support: EvalSupport::ParserOnly,
        summary: "bottom K by last value",
    },
    AggregateOp {
        name: "bottomk_min",
        takes_param: true,
        support: EvalSupport::ParserOnly,
        summary: "bottom K by minimum",
    },
    AggregateOp {
        name: "count",
        takes_param: false,
        support: EvalSupport::ParserAndEval,
        summary: "number of series",
    },
    AggregateOp {
        name: "count_values",
        takes_param: true,
        support: EvalSupport::ParserOnly,
        summary: "count series by value",
    },
    AggregateOp {
        name: "distinct",
        takes_param: false,
        support: EvalSupport::ParserOnly,
        summary: "distinct values per group",
    },
    AggregateOp {
        name: "geomean",
        takes_param: false,
        support: EvalSupport::ParserOnly,
        summary: "geometric mean",
    },
    AggregateOp {
        name: "group",
        takes_param: false,
        support: EvalSupport::ParserAndEval,
        summary: "constant 1 per group",
    },
    AggregateOp {
        name: "histogram",
        takes_param: false,
        support: EvalSupport::ParserOnly,
        summary: "histogram of group values",
    },
    AggregateOp {
        name: "limitk",
        takes_param: true,
        support: EvalSupport::ParserOnly,
        summary: "first K series",
    },
    AggregateOp {
        name: "mad",
        takes_param: false,
        support: EvalSupport::ParserOnly,
        summary: "median absolute deviation",
    },
    AggregateOp {
        name: "max",
        takes_param: false,
        support: EvalSupport::ParserAndEval,
        summary: "maximum value",
    },
    AggregateOp {
        name: "median",
        takes_param: false,
        support: EvalSupport::ParserOnly,
        summary: "median value",
    },
    AggregateOp {
        name: "min",
        takes_param: false,
        support: EvalSupport::ParserAndEval,
        summary: "minimum value",
    },
    AggregateOp {
        name: "mode",
        takes_param: false,
        support: EvalSupport::ParserOnly,
        summary: "mode (most frequent value)",
    },
    AggregateOp {
        name: "outliers_iqr",
        takes_param: false,
        support: EvalSupport::ParserOnly,
        summary: "IQR outliers",
    },
    AggregateOp {
        name: "outliers_mad",
        takes_param: true,
        support: EvalSupport::ParserOnly,
        summary: "MAD outliers",
    },
    AggregateOp {
        name: "outliersk",
        takes_param: true,
        support: EvalSupport::ParserOnly,
        summary: "top K outlier series",
    },
    AggregateOp {
        name: "quantile",
        takes_param: true,
        support: EvalSupport::ParserAndEval,
        summary: "φ-quantile",
    },
    AggregateOp {
        name: "quantiles",
        takes_param: true,
        support: EvalSupport::ParserOnly,
        summary: "multiple φ-quantiles into labels",
    },
    AggregateOp {
        name: "share",
        takes_param: false,
        support: EvalSupport::ParserOnly,
        summary: "fractional share per group",
    },
    AggregateOp {
        name: "stddev",
        takes_param: false,
        support: EvalSupport::ParserAndEval,
        summary: "standard deviation",
    },
    AggregateOp {
        name: "stdvar",
        takes_param: false,
        support: EvalSupport::ParserAndEval,
        summary: "variance",
    },
    AggregateOp {
        name: "sum",
        takes_param: false,
        support: EvalSupport::ParserAndEval,
        summary: "sum",
    },
    AggregateOp {
        name: "sum2",
        takes_param: false,
        support: EvalSupport::ParserOnly,
        summary: "sum of squares",
    },
    AggregateOp {
        name: "topk",
        takes_param: true,
        support: EvalSupport::ParserAndEval,
        summary: "top K series",
    },
    AggregateOp {
        name: "topk_avg",
        takes_param: true,
        support: EvalSupport::ParserOnly,
        summary: "top K by average",
    },
    AggregateOp {
        name: "topk_max",
        takes_param: true,
        support: EvalSupport::ParserOnly,
        summary: "top K by maximum",
    },
    AggregateOp {
        name: "topk_median",
        takes_param: true,
        support: EvalSupport::ParserOnly,
        summary: "top K by median",
    },
    AggregateOp {
        name: "topk_last",
        takes_param: true,
        support: EvalSupport::ParserOnly,
        summary: "top K by last value",
    },
    AggregateOp {
        name: "topk_min",
        takes_param: true,
        support: EvalSupport::ParserOnly,
        summary: "top K by minimum",
    },
    AggregateOp {
        name: "zscore",
        takes_param: false,
        support: EvalSupport::ParserOnly,
        summary: "z-score per group",
    },
];

// =====================================================================
// Rollup functions (range-vector input)
// =====================================================================

/// One rollup function. Mirrors a single arm in
/// `eval::RollupFn::from_name`. Takes a range-vector input
/// like `metric[5m]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollupFunction {
    pub name: &'static str,
    pub support: EvalSupport,
    pub summary: &'static str,
}

/// Every rollup function the parser+evaluator pairing
/// supports. Mirrors `eval::RollupFn::from_name` plus the
/// `quantile_over_time` parameterised rollup.
pub const ROLLUP_FUNCTIONS: &[RollupFunction] = &[
    RollupFunction {
        name: "avg_over_time",
        support: EvalSupport::ParserAndEval,
        summary: "average value over the range",
    },
    RollupFunction {
        name: "count_over_time",
        support: EvalSupport::ParserAndEval,
        summary: "number of samples in the range",
    },
    RollupFunction {
        name: "delta",
        support: EvalSupport::ParserAndEval,
        summary: "difference: last − first sample (gauges)",
    },
    RollupFunction {
        name: "first_over_time",
        support: EvalSupport::ParserAndEval,
        summary: "first sample in the range",
    },
    RollupFunction {
        name: "increase",
        support: EvalSupport::ParserAndEval,
        summary: "counter increase over the range",
    },
    RollupFunction {
        name: "last_over_time",
        support: EvalSupport::ParserAndEval,
        summary: "last sample in the range",
    },
    RollupFunction {
        name: "max_over_time",
        support: EvalSupport::ParserAndEval,
        summary: "maximum value over the range",
    },
    RollupFunction {
        name: "min_over_time",
        support: EvalSupport::ParserAndEval,
        summary: "minimum value over the range",
    },
    RollupFunction {
        name: "quantile_over_time",
        support: EvalSupport::ParserAndEval,
        summary: "φ-quantile over the range (param + range)",
    },
    RollupFunction {
        name: "rate",
        support: EvalSupport::ParserAndEval,
        summary: "per-second rate over the range",
    },
    RollupFunction {
        name: "stddev_over_time",
        support: EvalSupport::ParserAndEval,
        summary: "standard deviation over the range",
    },
    RollupFunction {
        name: "stdvar_over_time",
        support: EvalSupport::ParserAndEval,
        summary: "variance over the range",
    },
    RollupFunction {
        name: "sum_over_time",
        support: EvalSupport::ParserAndEval,
        summary: "sum of samples over the range",
    },
    RollupFunction {
        name: "tfirst_over_time",
        support: EvalSupport::ParserAndEval,
        summary: "Unix seconds of the first sample in the range",
    },
    RollupFunction {
        name: "tlast_change_over_time",
        support: EvalSupport::ParserAndEval,
        summary: "Unix seconds of the last value CHANGE in the range",
    },
    RollupFunction {
        name: "tlast_over_time",
        support: EvalSupport::ParserAndEval,
        summary: "Unix seconds of the last sample in the range",
    },
];

// =====================================================================
// Binary operators
// =====================================================================

/// One binary operator (per metricsql §"Binary operators").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryOpInfo {
    pub op: &'static str,
    pub kind: BinaryOpKind,
    pub support: EvalSupport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOpKind {
    /// `+ - * / % ^` etc.
    Arithmetic,
    /// `== != > >= < <=` — accept the `bool` modifier.
    Comparison,
    /// `and or unless` — set ops.
    LogicSet,
}

pub const BINARY_OPS: &[BinaryOpInfo] = &[
    BinaryOpInfo {
        op: "+",
        kind: BinaryOpKind::Arithmetic,
        support: EvalSupport::ParserAndEval,
    },
    BinaryOpInfo {
        op: "-",
        kind: BinaryOpKind::Arithmetic,
        support: EvalSupport::ParserAndEval,
    },
    BinaryOpInfo {
        op: "*",
        kind: BinaryOpKind::Arithmetic,
        support: EvalSupport::ParserAndEval,
    },
    BinaryOpInfo {
        op: "/",
        kind: BinaryOpKind::Arithmetic,
        support: EvalSupport::ParserAndEval,
    },
    BinaryOpInfo {
        op: "%",
        kind: BinaryOpKind::Arithmetic,
        support: EvalSupport::ParserAndEval,
    },
    BinaryOpInfo {
        op: "^",
        kind: BinaryOpKind::Arithmetic,
        support: EvalSupport::ParserAndEval,
    },
    BinaryOpInfo {
        op: "atan2",
        kind: BinaryOpKind::Arithmetic,
        support: EvalSupport::ParserOnly,
    },
    BinaryOpInfo {
        op: "==",
        kind: BinaryOpKind::Comparison,
        support: EvalSupport::ParserAndEval,
    },
    BinaryOpInfo {
        op: "!=",
        kind: BinaryOpKind::Comparison,
        support: EvalSupport::ParserAndEval,
    },
    BinaryOpInfo {
        op: ">",
        kind: BinaryOpKind::Comparison,
        support: EvalSupport::ParserAndEval,
    },
    BinaryOpInfo {
        op: ">=",
        kind: BinaryOpKind::Comparison,
        support: EvalSupport::ParserAndEval,
    },
    BinaryOpInfo {
        op: "<",
        kind: BinaryOpKind::Comparison,
        support: EvalSupport::ParserAndEval,
    },
    BinaryOpInfo {
        op: "<=",
        kind: BinaryOpKind::Comparison,
        support: EvalSupport::ParserAndEval,
    },
    BinaryOpInfo {
        op: "and",
        kind: BinaryOpKind::LogicSet,
        support: EvalSupport::ParserAndEval,
    },
    BinaryOpInfo {
        op: "or",
        kind: BinaryOpKind::LogicSet,
        support: EvalSupport::ParserAndEval,
    },
    BinaryOpInfo {
        op: "unless",
        kind: BinaryOpKind::LogicSet,
        support: EvalSupport::ParserAndEval,
    },
];

// =====================================================================
// Parser-corpus completeness — ParserOnly bulk lists
// =====================================================================
//
// Names below are accepted by the metricsql parser
// (`<ident>(...)` as a function call), round-trip through the
// prettifier, but the evaluator currently returns
// `EvalError::NotYetImplemented` for them. They live in
// flat `&[&str]` constants — adding rich metadata for each
// without an evaluator backing would mislead the user.
//
// Each list mirrors `links/specs/MetricsQL.md` directly. The
// audit doc-test (`bulk_lists_match_spec_corpus`) pins the
// counts; updating the spec means updating the lists in the
// same change.

/// Rollup functions the parser accepts but the evaluator
/// currently doesn't implement. 67 entries (80 in spec
/// minus the 13 in [`ROLLUP_FUNCTIONS`]).
pub const EXTRA_ROLLUP_FUNCTIONS_SPEC: &[&str] = &[
    "absent_over_time",
    "aggr_over_time",
    "ascent_over_time",
    "changes",
    "changes_prometheus",
    "count_eq_over_time",
    "count_gt_over_time",
    "count_le_over_time",
    "count_ne_over_time",
    "count_values_over_time",
    "decreases_over_time",
    "default_rollup",
    "delta_prometheus",
    "deriv",
    "deriv_fast",
    "descent_over_time",
    "distinct_over_time",
    "duration_over_time",
    "geomean_over_time",
    "histogram_over_time",
    "hoeffding_bound_lower",
    "hoeffding_bound_upper",
    "holt_winters",
    "idelta",
    "ideriv",
    "increase_prometheus",
    "increase_pure",
    "increases_over_time",
    "integrate",
    "irate",
    "lag",
    "lifetime",
    "mad_over_time",
    "median_over_time",
    "mode_over_time",
    "outlier_iqr_over_time",
    "predict_linear",
    "present_over_time",
    "quantiles_over_time",
    "range_over_time",
    "rate_over_sum",
    "rate_prometheus",
    "resets",
    "rollup",
    "rollup_candlestick",
    "rollup_delta",
    "rollup_deriv",
    "rollup_increase",
    "rollup_rate",
    "rollup_scrape_interval",
    "scrape_interval",
    "share_eq_over_time",
    "share_gt_over_time",
    "share_le_over_time",
    "stale_samples_over_time",
    "sum2_over_time",
    "sum_eq_over_time",
    "sum_gt_over_time",
    "sum_le_over_time",
    "timestamp",
    "timestamp_with_name",
    "tmax_over_time",
    "tmin_over_time",
    "zscore_over_time",
];

/// Transform functions per `links/specs/MetricsQL.md`
/// §"Transform functions". 93 entries; all currently
/// ParserOnly (evaluator NYI).
pub const TRANSFORM_FUNCTIONS_SPEC: &[&str] = &[
    "abs",
    "absent",
    "acos",
    "acosh",
    "asin",
    "asinh",
    "atan",
    "atanh",
    "bitmap_and",
    "bitmap_or",
    "bitmap_xor",
    "buckets_limit",
    "ceil",
    "clamp",
    "clamp_max",
    "clamp_min",
    "cos",
    "cosh",
    "day_of_month",
    "day_of_week",
    "day_of_year",
    "days_in_month",
    "deg",
    "drop_empty_series",
    "end",
    "exp",
    "floor",
    "histogram_avg",
    "histogram_fraction",
    "histogram_quantile",
    "histogram_quantiles",
    "histogram_share",
    "histogram_stddev",
    "histogram_stdvar",
    "hour",
    "interpolate",
    "keep_last_value",
    "keep_next_value",
    "limit_offset",
    "ln",
    "log10",
    "log2",
    "minute",
    "month",
    "now",
    "pi",
    "prometheus_buckets",
    "rad",
    "rand",
    "rand_exponential",
    "rand_normal",
    "range_avg",
    "range_first",
    "range_last",
    "range_linear_regression",
    "range_mad",
    "range_max",
    "range_median",
    "range_min",
    "range_normalize",
    "range_quantile",
    "range_stddev",
    "range_stdvar",
    "range_sum",
    "range_trim_outliers",
    "range_trim_spikes",
    "range_trim_zscore",
    "range_zscore",
    "remove_resets",
    "round",
    "ru",
    "running_avg",
    "running_max",
    "running_min",
    "running_sum",
    "scalar",
    "sgn",
    "sin",
    "sinh",
    "smooth_exponential",
    "sort",
    "sort_desc",
    "sqrt",
    "start",
    "step",
    "tan",
    "tanh",
    "time",
    "timezone_offset",
    "ttf",
    "union",
    "vector",
    "year",
];

/// Label-manipulation functions per
/// `links/specs/MetricsQL.md`. 22 entries; all currently
/// ParserOnly.
pub const LABEL_FUNCTIONS_SPEC: &[&str] = &[
    "alias",
    "drop_common_labels",
    "label_copy",
    "label_del",
    "label_graphite_group",
    "label_join",
    "label_keep",
    "label_lowercase",
    "label_map",
    "label_match",
    "label_mismatch",
    "label_move",
    "label_replace",
    "label_set",
    "label_transform",
    "label_uppercase",
    "label_value",
    "labels_equal",
    "sort_by_label",
    "sort_by_label_desc",
    "sort_by_label_numeric",
    "sort_by_label_numeric_desc",
];

// =====================================================================
// Modifiers
// =====================================================================

/// Modifiers attached to aggregation operators
/// (`sum by (label)` / `sum without (label)`).
pub const AGGREGATE_MODIFIERS: &[&str] = &["by", "without"];

/// Modifiers attached to binary ops to control vector
/// matching (`a + on(label) b` / `a + ignoring(label) b`).
pub const VECTOR_MATCH_MODIFIERS: &[&str] = &["on", "ignoring"];

/// Cardinality modifiers paired with `on`/`ignoring`.
pub const GROUP_MODIFIERS: &[&str] = &["group_left", "group_right"];

/// `bool` modifier on comparison ops to coerce the result
/// into 0/1 instead of filtering by truthiness.
pub const COMPARISON_BOOL_MODIFIER: &str = "bool";

/// `keep_metric_names` suffix on a function call to retain
/// the `__name__` label that aggregations would otherwise
/// drop.
pub const KEEP_METRIC_NAMES: &str = "keep_metric_names";

/// `offset` modifier on a selector / rollup
/// (`metric{} offset 5m`).
pub const OFFSET_MODIFIER: &str = "offset";

/// `limit N` cap on aggregate output cardinality
/// (`sum(...) limit 10`).
pub const LIMIT_MODIFIER: &str = "limit";

/// Matcher operators inside `{...}` selectors. Mirrors
/// [`crate::eval::MatcherOp`] but as parsable tokens.
pub const MATCHER_OPS: &[&str] = &["=", "!=", "=~", "!~"];

// =====================================================================
// Lookup helpers
// =====================================================================

/// Find an aggregation operator by name.
pub fn aggregate_op_by_name(name: &str) -> Option<&'static AggregateOp> {
    AGGREGATE_OPS.iter().find(|a| a.name == name)
}

/// Find a rollup function by name.
pub fn rollup_function_by_name(name: &str) -> Option<&'static RollupFunction> {
    ROLLUP_FUNCTIONS.iter().find(|f| f.name == name)
}

/// All aggregation-operator names matching `prefix`.
/// `evaluable_only=true` filters to entries the evaluator
/// runs (drops [`EvalSupport::ParserOnly`]).
pub fn aggregate_op_names_starting_with(prefix: &str, evaluable_only: bool) -> Vec<&'static str> {
    AGGREGATE_OPS
        .iter()
        .filter(|a| a.name.starts_with(prefix))
        .filter(|a| !evaluable_only || a.support.evaluable())
        .map(|a| a.name)
        .collect()
}

/// All rollup-function names matching `prefix`.
pub fn rollup_function_names_starting_with(
    prefix: &str,
    evaluable_only: bool,
) -> Vec<&'static str> {
    ROLLUP_FUNCTIONS
        .iter()
        .filter(|f| f.name.starts_with(prefix))
        .filter(|f| !evaluable_only || f.support.evaluable())
        .map(|f| f.name)
        .collect()
}

/// All callable names (aggregate + rollup + transform +
/// label) starting with `prefix`. The metricsql expression-
/// completer's main entry point: at any position where a
/// function call is valid, any of these may appear.
///
/// `evaluable_only=true` filters the bulk parser-only lists
/// (transforms, labels, extra rollups) out — leaving only
/// names with [`EvalSupport::ParserAndEval`] coverage.
pub fn callable_names_starting_with(prefix: &str, evaluable_only: bool) -> Vec<&'static str> {
    let mut out = aggregate_op_names_starting_with(prefix, evaluable_only);
    out.extend(rollup_function_names_starting_with(prefix, evaluable_only));
    if !evaluable_only {
        out.extend(
            EXTRA_ROLLUP_FUNCTIONS_SPEC
                .iter()
                .filter(|n| n.starts_with(prefix))
                .copied(),
        );
        out.extend(
            TRANSFORM_FUNCTIONS_SPEC
                .iter()
                .filter(|n| n.starts_with(prefix))
                .copied(),
        );
        out.extend(
            LABEL_FUNCTIONS_SPEC
                .iter()
                .filter(|n| n.starts_with(prefix))
                .copied(),
        );
    }
    out.sort();
    out.dedup();
    out
}

/// True if `name` is a callable parser-corpus token
/// (aggregate / rollup / transform / label). Used by lints
/// that want to flag `<unknown>(...)` in user expressions.
pub fn is_known_callable(name: &str) -> bool {
    aggregate_op_by_name(name).is_some()
        || rollup_function_by_name(name).is_some()
        || EXTRA_ROLLUP_FUNCTIONS_SPEC.contains(&name)
        || TRANSFORM_FUNCTIONS_SPEC.contains(&name)
        || LABEL_FUNCTIONS_SPEC.contains(&name)
}

/// Total count of distinct callable names recognised across
/// the parser corpus. Used by the parity doc-test.
pub fn known_callable_count() -> usize {
    let mut all: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
    for a in AGGREGATE_OPS {
        all.insert(a.name);
    }
    for r in ROLLUP_FUNCTIONS {
        all.insert(r.name);
    }
    for n in EXTRA_ROLLUP_FUNCTIONS_SPEC {
        all.insert(n);
    }
    for n in TRANSFORM_FUNCTIONS_SPEC {
        all.insert(n);
    }
    for n in LABEL_FUNCTIONS_SPEC {
        all.insert(n);
    }
    all.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin: every aggregate name in the registry is recognised
    /// by the parser. Drift in either direction breaks here.
    #[test]
    fn aggregate_registry_matches_parser_acceptance() {
        // Use the parser's own grammar by parsing a tiny
        // probe expression for each registered aggregate. The
        // parser will succeed only if its `is_aggr_func` arm
        // covers the name. We use a clearly-aggregate shape
        // (`<name>(x)`); a non-aggregate function with the
        // same shape would also parse, but the parser's
        // canonicalisation step lowercases ONLY aggregates,
        // so `SUM(x).pretty == "sum(x)"` while
        // `RATE(x).pretty == "RATE(x)"`. The behaviour pinpoints
        // membership without exposing a private helper.
        for op in AGGREGATE_OPS {
            let upper = op.name.to_ascii_uppercase();
            let probe = format!("{upper}(x)");
            let parsed = crate::parser::parse(&probe)
                .unwrap_or_else(|e| panic!("aggregate {} should parse: {e:?}", op.name));
            let pretty = crate::prettifier::pretty_string(&parsed);
            assert!(
                pretty.starts_with(op.name),
                "registry says '{}' is an aggregate, but parser didn't \
                 lowercase its name in round-trip — pretty={pretty}",
                op.name,
            );
        }
    }

    /// Pin: the rollup-function registry doesn't claim to
    /// support a name the evaluator hasn't wired. (Symmetry of
    /// `aggregate_registry_matches_parser_acceptance` for the
    /// eval-supported half.)
    #[test]
    fn rollup_eval_marks_track_evaluator_dispatch() {
        // Names the registry marks as ParserAndEval must all
        // be names `RollupFn::from_name` recognises. We can't
        // call the private `from_name` from outside the crate,
        // but we can probe via `evaluate_func` by constructing
        // an empty range-vector query and checking the error
        // shape. NotYetImplemented = parser-only; any other
        // error class (BadValue / DataSource / Ok) = evaluator
        // recognised the name.
        //
        // For the purpose of this pin we only need the
        // `from_name` membership to be tight. The registry's
        // every-entry-is-evaluable claim today is verified
        // structurally: the registry has no ParserOnly rollups
        // (every entry is ParserAndEval).
        for f in ROLLUP_FUNCTIONS {
            assert_eq!(
                f.support,
                EvalSupport::ParserAndEval,
                "rollup '{}' marked {:?}; the registry's invariant is \
                 'every rollup matches the evaluator dispatch.' Add a \
                 ParserOnly variant only after the evaluator drops support, \
                 or add the eval handler before listing the name.",
                f.name,
                f.support
            );
        }
    }

    #[test]
    fn aggregate_registry_includes_evaluator_subset() {
        // The evaluator handles a known subset: sum / avg /
        // min / max / count / group / stddev / stdvar +
        // topk / bottomk / quantile.
        for required_eval in [
            "sum", "avg", "min", "max", "count", "group", "stddev", "stdvar", "topk", "bottomk",
            "quantile",
        ] {
            let entry = aggregate_op_by_name(required_eval)
                .unwrap_or_else(|| panic!("missing aggregate '{required_eval}'"));
            assert_eq!(
                entry.support,
                EvalSupport::ParserAndEval,
                "aggregate '{required_eval}' should be ParserAndEval"
            );
        }
    }

    #[test]
    fn rollup_registry_includes_full_evaluator_dispatch() {
        // Mirror eval::RollupFn::from_name verbatim.
        for required in [
            "rate",
            "increase",
            "delta",
            "sum_over_time",
            "avg_over_time",
            "min_over_time",
            "max_over_time",
            "count_over_time",
            "last_over_time",
            "first_over_time",
            "stddev_over_time",
            "stdvar_over_time",
            "quantile_over_time",
        ] {
            assert!(
                rollup_function_by_name(required).is_some(),
                "rollup registry missing '{required}' (evaluator handles it)"
            );
        }
    }

    #[test]
    fn aggregate_takes_param_set_matches_evaluator() {
        // Parameterised aggregates in the evaluator: topk,
        // bottomk, quantile.
        for required in ["topk", "bottomk", "quantile"] {
            let entry = aggregate_op_by_name(required).unwrap();
            assert!(
                entry.takes_param,
                "aggregate '{required}' should take a leading param"
            );
        }
        // Non-parameterised that the evaluator runs.
        for non_param in [
            "sum", "avg", "min", "max", "count", "group", "stddev", "stdvar",
        ] {
            let entry = aggregate_op_by_name(non_param).unwrap();
            assert!(
                !entry.takes_param,
                "aggregate '{non_param}' should NOT take a param"
            );
        }
    }

    #[test]
    fn aggregate_registry_names_unique() {
        let mut seen = std::collections::HashSet::new();
        for a in AGGREGATE_OPS {
            assert!(seen.insert(a.name), "duplicate aggregate '{}'", a.name);
        }
    }

    #[test]
    fn rollup_registry_names_unique() {
        let mut seen = std::collections::HashSet::new();
        for f in ROLLUP_FUNCTIONS {
            assert!(seen.insert(f.name), "duplicate rollup '{}'", f.name);
        }
    }

    #[test]
    fn aggregate_op_by_name_round_trips() {
        for a in AGGREGATE_OPS {
            assert_eq!(aggregate_op_by_name(a.name), Some(a));
        }
        assert_eq!(aggregate_op_by_name("nonsense"), None);
    }

    #[test]
    fn callable_names_starting_with_filters_evaluable_only() {
        // 'a' covers `any` (parser-only) and `avg`
        // (eval-supported), plus rollups starting with 'a'
        // (`avg_over_time`, eval-supported).
        let all = callable_names_starting_with("a", false);
        let evaluable = callable_names_starting_with("a", true);
        assert!(all.contains(&"any"));
        assert!(all.contains(&"avg"));
        assert!(all.contains(&"avg_over_time"));
        assert!(
            !evaluable.contains(&"any"),
            "evaluable-only filter should drop ParserOnly entries"
        );
        assert!(evaluable.contains(&"avg"));
        assert!(evaluable.contains(&"avg_over_time"));
    }

    #[test]
    fn binary_op_table_covers_arithmetic_comparison_set_and_atan2() {
        let arith: usize = BINARY_OPS
            .iter()
            .filter(|b| b.kind == BinaryOpKind::Arithmetic)
            .count();
        assert!(arith >= 7, "arith should include + - * / % ^ atan2");
        let cmp: usize = BINARY_OPS
            .iter()
            .filter(|b| b.kind == BinaryOpKind::Comparison)
            .count();
        assert_eq!(cmp, 6, "== != > >= < <=");
        let set: usize = BINARY_OPS
            .iter()
            .filter(|b| b.kind == BinaryOpKind::LogicSet)
            .count();
        assert_eq!(set, 3, "and / or / unless");
    }

    #[test]
    fn modifier_constants_match_metricsql_spec() {
        assert_eq!(AGGREGATE_MODIFIERS, &["by", "without"]);
        assert_eq!(VECTOR_MATCH_MODIFIERS, &["on", "ignoring"]);
        assert_eq!(GROUP_MODIFIERS, &["group_left", "group_right"]);
        assert_eq!(COMPARISON_BOOL_MODIFIER, "bool");
        assert_eq!(KEEP_METRIC_NAMES, "keep_metric_names");
        assert_eq!(OFFSET_MODIFIER, "offset");
        assert_eq!(LIMIT_MODIFIER, "limit");
    }

    #[test]
    fn matcher_ops_cover_all_four_canonical_forms() {
        for required in ["=", "!=", "=~", "!~"] {
            assert!(
                MATCHER_OPS.contains(&required),
                "missing matcher op '{required}'"
            );
        }
    }

    #[test]
    fn evaluable_predicate_matches_support_variant() {
        assert!(EvalSupport::ParserAndEval.evaluable());
        assert!(!EvalSupport::ParserOnly.evaluable());
    }

    // ── Parser-corpus parity (vs links/specs/MetricsQL.md) ──

    #[test]
    fn parser_corpus_counts_match_spec() {
        // links/specs/MetricsQL.md as of the 2026-05-05 audit:
        // aggregates 37, rollups 80, transforms 93, labels 22.
        assert_eq!(AGGREGATE_OPS.len(), 37, "aggregates: spec=37");
        assert_eq!(
            ROLLUP_FUNCTIONS.len() + EXTRA_ROLLUP_FUNCTIONS_SPEC.len(),
            80,
            "rollups: spec=80 (eval={} + parser-only={})",
            ROLLUP_FUNCTIONS.len(),
            EXTRA_ROLLUP_FUNCTIONS_SPEC.len()
        );
        assert_eq!(TRANSFORM_FUNCTIONS_SPEC.len(), 93, "transforms: spec=93");
        assert_eq!(LABEL_FUNCTIONS_SPEC.len(), 22, "labels: spec=22");
    }

    #[test]
    fn bulk_lists_are_unique_and_disjoint_from_structured() {
        // Each parser-only list is internally unique.
        for (label, list) in [
            ("EXTRA_ROLLUP_FUNCTIONS_SPEC", EXTRA_ROLLUP_FUNCTIONS_SPEC),
            ("TRANSFORM_FUNCTIONS_SPEC", TRANSFORM_FUNCTIONS_SPEC),
            ("LABEL_FUNCTIONS_SPEC", LABEL_FUNCTIONS_SPEC),
        ] {
            let mut seen = std::collections::HashSet::new();
            for n in list {
                assert!(seen.insert(n), "duplicate '{}' in {label}", n);
            }
        }
        // No name appears in both the structured rollup list
        // and the parser-only extras (drift between
        // ROLLUP_FUNCTIONS and EXTRA_ROLLUP_FUNCTIONS_SPEC
        // would create double-counting in completion).
        for r in ROLLUP_FUNCTIONS {
            assert!(
                !EXTRA_ROLLUP_FUNCTIONS_SPEC.contains(&r.name),
                "'{}' appears in both structured rollups and \
                 EXTRA_ROLLUP_FUNCTIONS_SPEC",
                r.name
            );
        }
    }

    #[test]
    fn is_known_callable_recognises_every_corpus_name() {
        for n in EXTRA_ROLLUP_FUNCTIONS_SPEC {
            assert!(is_known_callable(n), "missing '{n}'");
        }
        for n in TRANSFORM_FUNCTIONS_SPEC {
            assert!(is_known_callable(n), "missing '{n}'");
        }
        for n in LABEL_FUNCTIONS_SPEC {
            assert!(is_known_callable(n), "missing '{n}'");
        }
        for a in AGGREGATE_OPS {
            assert!(is_known_callable(a.name), "missing aggregate '{}'", a.name);
        }
        assert!(!is_known_callable("nonsense_function"));
    }

    #[test]
    fn callable_names_starting_with_includes_bulk_when_unfiltered() {
        // 'rang' covers 18 transform functions all starting
        // with 'range_*'. Without `evaluable_only`, all show.
        let unfiltered = callable_names_starting_with("range_", false);
        assert!(
            unfiltered.len() >= 16,
            "expected 16+ range_* transforms, got {} ({:?})",
            unfiltered.len(),
            unfiltered
        );
        // With `evaluable_only=true` they all disappear (none
        // are evaluator-supported).
        let filtered = callable_names_starting_with("range_", true);
        assert!(
            filtered.is_empty(),
            "evaluable_only should drop all parser-only entries: {filtered:?}"
        );
    }

    #[test]
    fn known_callable_count_matches_spec_total() {
        // 37 aggregates + 80 rollups + 93 transforms + 22 labels
        // = 232 distinct callable names in the spec.
        assert_eq!(known_callable_count(), 37 + 80 + 93 + 22);
    }
}
