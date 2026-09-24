// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Result validation and relevancy measurement (SRD 47).
//!
//! Provides `ValidatingDispenser`, a composable op dispenser wrapper
//! that verifies operation results against expected values and computes
//! information retrieval metrics (recall@k, precision@k, etc.).
//!
//! Applied only to templates that declare `verify:` or `relevancy:`
//! blocks — zero overhead for templates without validation.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nmbrs_metrics::labels::Labels;

use crate::adapter::{ExecutionError, OpDispenser, OpResult, WrappingDispenser};
use crate::relevancy::{self, RelevancyFn};
use crate::wires::WireSource;
use crate::wrapper_registry::{WrapperName, WrapperRegistration, WrapperSubject};

/// SRD-32a wrapper name for the validation layer. Exposed at
/// the module level so other wrappers' `forbids_outer` /
/// `requires_inner` slices can reference it without depending
/// on `wrapper_registrations` for the constant.
pub const WRAPPER_NAME: WrapperName = WrapperName::new("validate");

/// Trigger: op carries `verify:` or `relevancy:`.
fn wrapper_triggers(s: WrapperSubject) -> bool {
    let Some(template) = s.op() else {
        return false;
    };
    template.params.contains_key("verify") || template.params.contains_key("relevancy")
}

fn wrapper_describe_assignment(s: WrapperSubject) -> Option<String> {
    let template = s.op()?;
    let strict = template
        .params
        .get("strict")
        .and_then(|v| v.as_bool().or_else(|| v.as_str().map(|s| s == "true")))
        .unwrap_or(false);
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = template.params.get("verify") {
        parts.push(format!(
            "verify={}",
            crate::wrapper_registrations::short_value(v)
        ));
    }
    if let Some(v) = template.params.get("relevancy") {
        parts.push(format!(
            "relevancy={}",
            crate::wrapper_registrations::short_value(v)
        ));
    }
    if parts.is_empty() {
        return None;
    }
    let body = parts.join(", ");
    Some(if strict {
        format!("validate: {body} (strict)")
    } else {
        format!("validate: {body}")
    })
}

inventory::submit! {
    WrapperRegistration {
        name: WRAPPER_NAME,
        owned_fields: &["verify", "relevancy", "strict"],
        triggers: wrapper_triggers,
        requires_inner: &[crate::wrappers::traverse::NAME],
        forbids_outer: &[],
        mutually_exclusive_with: &[],
        describe_assignment: wrapper_describe_assignment,
        levels: &[crate::wrapper_registry::WrapperLevel::Op],
    }
}

// =========================================================================
// Validation configuration (parsed from workload YAML at init time)
// =========================================================================

/// Closed vocabulary of op-template `params:` keys the runtime
/// itself consumes (validation, batching, polling, op weighting,
/// adapter selection). Joined at validation time with
/// [`crate::runner::KNOWN_PARAMS`] (workload/CLI-level keys that
/// get blast-merged into every op's params during parse) and the
/// adapter's
/// [`crate::adapter::DriverAdapter::known_op_params`] declarations
/// to form the full allow-list. Unknown keys are rejected at op
/// setup time so silent-ignore traps like `evaluations: { relevancy: ... }`
/// (a wrapper key the runtime never reads) cannot hide a
/// misconfigured op.
/// Core op-template params consumed directly by the runtime or the
/// adapter layer that are NOT owned by any wrapper. Wrapper fields
/// (`verify`, `poll`, `memo`, `readout`, `errors`, `tries`, `while`,
/// `rate`, `fields`, …) are deliberately absent: the op
/// closed-vocabulary guard accepts them via
/// [`crate::wrapper_registry::WrapperRegistry::owns_field`], which
/// derives the wrapper vocabulary structurally from each wrapper's
/// `owned_fields` declaration. Keeping them out of this list is the
/// point — one source of truth per field, so adding a wrapper never
/// requires editing here. The drift-guard test
/// `core_op_params_disjoint_from_owned_fields` fails if a wrapper
/// field creeps back in.
pub const CORE_OP_PARAMS: &[&str] = &[
    // Batching (no wrapper owns these — consumed by the batch path).
    "batch",
    "batchtype",
    "max_batch_size",
    // Op weighting (activity-level dispatch weight, not a wrapper).
    "ratio",
    // Adapter selection.
    "adapter",
    "driver",
    // Daemon-op declaration. `daemon` marks an op for cycle-pool
    // dispatch onto a daemon fiber (with per-op-name cap);
    // `daemon_cancel_grace_ms` overrides the phase-exit drain budget.
    // (The loop / rate primitives `while` / `rate` ARE wrapper-owned
    // and come from the registry.)
    "daemon",
    "daemon_cancel_grace_ms",
];

/// Configuration for relevancy measurement on a single op template.
#[derive(Debug, Clone)]
pub struct RelevancyConfig {
    /// Column/field name to extract actual result indices from.
    pub actual_field: String,
    /// Polydat binding name that produces ground truth indices.
    pub expected_binding: String,
    /// Recall window — number of top results used in the @k
    /// metric computation. The first `k` of `actual` are
    /// compared against the first `k` of `expected`.
    pub k: usize,
    /// Optional retrieval window. When set, declares "we
    /// retrieved exactly `r` results, but score using only the
    /// first `k` of them" (the "k-recall@r" semantic). Runtime
    /// asserts `actual.len() == r`. When `None`, no separate
    /// retrieval window is enforced — actual is truncated to
    /// `k` for the comparison and any length is accepted (the
    /// pre-`r` behavior).
    pub r: Option<usize>,
    /// Which functions to compute.
    pub functions: Vec<RelevancyFn>,
}

/// A single field assertion from a `verify:` block.
#[derive(Debug, Clone)]
pub struct AssertionSpec {
    /// Field name to check in the result.
    pub field: String,
    /// The predicate to apply.
    pub predicate: AssertionPredicate,
}

/// Predicate for field assertion checks.
#[derive(Debug, Clone)]
pub enum AssertionPredicate {
    /// Field must equal this value (string comparison).
    Eq(String),
    /// Field must not be null/absent.
    NotNull,
    /// Field must be null/absent.
    IsNull,
    /// Numeric: field >= threshold.
    Gte(f64),
    /// Numeric: field <= threshold.
    Lte(f64),
    /// String: field contains substring.
    Contains(String),
    /// A numeric bound (`gte:` / `lte:`) whose value is not a number.
    ///
    /// Carried rather than dropped, and ALWAYS fails: the previous
    /// `as_f64().unwrap_or(0.0)` silently turned `lte: "5"` — or any
    /// unresolved placeholder — into `<= 0`, which then failed against
    /// perfectly good data with a message quoting a threshold the author
    /// never wrote. A bound nobody can evaluate must say so, not pick zero.
    MalformedBound { key: String, raw: String },
    /// Body-level: result must contain at least N rows
    /// (`element_count() >= N`). Use to make a SELECT
    /// failsafe — empty result sets surface as a hard error
    /// instead of silently passing per-row assertions vacuously.
    /// Field name is ignored for this predicate.
    MinRows(u64),
}

impl AssertionSpec {
    /// Check this assertion against a result.
    pub fn check(&self, result: &OpResult) -> bool {
        // Body-level predicates are evaluated against the result's
        // shape rather than a per-row field value. Handled before
        // field extraction.
        if let AssertionPredicate::MinRows(n) = &self.predicate {
            let row_count = result.body.as_ref().map(|b| b.element_count()).unwrap_or(0);
            return row_count >= *n;
        }

        let json = match &result.body {
            Some(body) => body.to_json(),
            // No body at all. A field predicate constrains the VALUE of
            // something in the response, so with no response it is vacuous —
            // it passes rather than failing. A succeeding op that returns
            // nothing (HTTP 204, or `on_timeout: accept`, where the server is
            // still working and the poll layer is the real observer) must not
            // be failed by a `field: status, eq: 200` clause that was written
            // to check a body when one arrives.
            //
            // Requiring a body is available, but must be ASKED for, and the
            // two spellings already exist:
            //   `is: not_null`  — this field must be present
            //   `min_rows: 1`   — the result must carry at least one row
            // Both of those constrain PRESENCE, not value, so both still fail
            // here. That is the whole distinction: value predicates go quiet
            // when there is nothing to read; presence predicates are exactly
            // the ones that shouldn't.
            None => {
                return !matches!(
                    self.predicate,
                    AssertionPredicate::NotNull | AssertionPredicate::MalformedBound { .. }
                );
            }
        };

        let field_val = extract_field_from_json(&json, &self.field);

        match &self.predicate {
            AssertionPredicate::NotNull => field_val.is_some(),
            AssertionPredicate::IsNull => field_val.is_none(),
            AssertionPredicate::Eq(expected) => {
                field_val.is_some_and(|v| json_value_as_string(v) == *expected)
            }
            AssertionPredicate::Gte(threshold) => field_val
                .and_then(|v| v.as_f64())
                .is_some_and(|v| v >= *threshold),
            AssertionPredicate::Lte(threshold) => field_val
                .and_then(|v| v.as_f64())
                .is_some_and(|v| v <= *threshold),
            AssertionPredicate::Contains(substr) => {
                field_val.is_some_and(|v| json_value_as_string(v).contains(substr.as_str()))
            }
            AssertionPredicate::MalformedBound { .. } => false,
            AssertionPredicate::MinRows(_) => unreachable!("MinRows handled in early-return above"),
        }
    }
}

// =========================================================================
// ValidationMetrics
// =========================================================================

/// Running aggregate per relevancy metric. Holds both the all-time
/// running mean and a bounded sliding window so progress views can
/// show recent trend alongside the cumulative average.
pub struct RunningAgg {
    /// Sum of every score ever recorded (for all-time mean).
    pub total_sum: f64,
    /// Count of scores recorded (for all-time mean).
    pub total_count: u64,
    /// Recent scores, newest at the back. Capped at `window_size`.
    pub window: std::collections::VecDeque<f64>,
    /// Upper bound on `window` length.
    pub window_size: usize,
}

impl RunningAgg {
    pub fn new(window_size: usize) -> Self {
        Self {
            total_sum: 0.0,
            total_count: 0,
            window: std::collections::VecDeque::with_capacity(window_size),
            window_size,
        }
    }

    pub fn record(&mut self, score: f64) {
        self.total_sum += score;
        self.total_count += 1;
        if self.window.len() == self.window_size {
            self.window.pop_front();
        }
        self.window.push_back(score);
    }

    pub fn window_mean(&self) -> f64 {
        if self.window.is_empty() {
            0.0
        } else {
            self.window.iter().sum::<f64>() / self.window.len() as f64
        }
    }

    pub fn total_mean(&self) -> f64 {
        if self.total_count == 0 {
            0.0
        } else {
            self.total_sum / self.total_count as f64
        }
    }
}

/// Default size of the moving-average window. Chosen to match the
/// common recall@10 "last 10" semantic the user's workloads expect.
pub const DEFAULT_RECALL_WINDOW: usize = 10;

/// Live snapshot of a relevancy metric: window mean, all-time mean,
/// how many scores have been recorded, and the current window size.
#[derive(Debug, Clone)]
pub struct RelevancyLive {
    pub name: String,
    pub window_mean: f64,
    pub total_mean: f64,
    pub total_count: u64,
    pub window_len: usize,
}

/// Metrics for result validation, shared across fibers.
pub struct ValidationMetrics {
    pub validations_passed: AtomicU64,
    pub validations_failed: AtomicU64,
    /// Per-function lossless f64 statistics accumulators for relevancy scores.
    /// Exact precision — no quantization, no bucket rounding.
    pub relevancy_stats: HashMap<String, nmbrs_metrics::summaries::f64stats::F64Stats>,
    /// Live aggregates (moving-window + all-time) per relevancy metric.
    /// These read non-destructively, so progress views can sample them
    /// every frame without disturbing the exact-precision stats above.
    pub running_aggregates: HashMap<String, std::sync::Mutex<RunningAgg>>,
}

impl ValidationMetrics {
    /// Create metrics for the given relevancy functions.
    ///
    /// The metric family name is the bare function name
    /// (`recall`, `precision`, `f1`, …) — `k` and `r` are
    /// carried as labels rather than baked into the
    /// family identifier. This collapses every per-`k`
    /// variant into a single family that consumers can
    /// query with `recall{k="10",r="10"}` instead of the
    /// awkward `recall_at_10` synthesised name. `r`
    /// defaults to `k` when the relevancy config doesn't
    /// declare it (`r:` was unset → first-k semantics).
    pub fn new(labels: &Labels, functions: &[RelevancyFn], k: usize, r: Option<usize>) -> Self {
        let r_value = r.unwrap_or(k);
        let stats_labels = labels
            .with("k", k.to_string())
            .with("r", r_value.to_string());
        let mut stats = HashMap::new();
        let mut running = HashMap::new();
        for func in functions {
            let metric_name = func.metric_name().to_string();
            stats.insert(
                metric_name.clone(),
                nmbrs_metrics::summaries::f64stats::F64Stats::new(
                    stats_labels.with("name", &metric_name),
                ),
            );
            running.insert(
                metric_name.clone(),
                std::sync::Mutex::new(RunningAgg::new(DEFAULT_RECALL_WINDOW)),
            );
        }
        Self {
            validations_passed: AtomicU64::new(0),
            validations_failed: AtomicU64::new(0),
            relevancy_stats: stats,
            running_aggregates: running,
        }
    }

    /// Create metrics with no relevancy functions (assertions only).
    pub fn assertions_only() -> Self {
        Self {
            validations_passed: AtomicU64::new(0),
            validations_failed: AtomicU64::new(0),
            relevancy_stats: HashMap::new(),
            running_aggregates: HashMap::new(),
        }
    }

    /// Record a relevancy score for a named function. Updates both the
    /// lossless stats accumulator and the live running aggregate.
    pub fn record_relevancy(&self, metric_name: &str, score: f64) {
        if let Some(stats) = self.relevancy_stats.get(metric_name) {
            stats.record(score);
        }
        if let Some(agg) = self.running_aggregates.get(metric_name) {
            let mut a = agg.lock().unwrap_or_else(|e| e.into_inner());
            a.record(score);
        }
    }

    /// Snapshot all live relevancy aggregates without disturbing the
    /// accumulators. Safe to call every frame from the progress thread.
    pub fn live_snapshot(&self) -> Vec<RelevancyLive> {
        let mut out = Vec::with_capacity(self.running_aggregates.len());
        for (name, agg) in &self.running_aggregates {
            let a = agg.lock().unwrap_or_else(|e| e.into_inner());
            out.push(RelevancyLive {
                name: name.clone(),
                window_mean: a.window_mean(),
                total_mean: a.total_mean(),
                total_count: a.total_count,
                window_len: a.window.len(),
            });
        }
        out.sort_by(|x, y| x.name.cmp(&y.name));
        out
    }

    /// Get pass count.
    pub fn passed(&self) -> u64 {
        self.validations_passed.load(Ordering::Relaxed)
    }

    /// Get fail count.
    pub fn failed(&self) -> u64 {
        self.validations_failed.load(Ordering::Relaxed)
    }
}

// =========================================================================
// ValidatingDispenser
// =========================================================================

/// Op dispenser wrapper that validates results after execution.
///
/// Applied only to templates that declare `verify:` or `relevancy:`
/// blocks. Zero overhead for templates without validation.
pub struct ValidatingDispenser {
    inner: Arc<dyn OpDispenser>,
    assertions: Vec<AssertionSpec>,
    relevancy: Option<RelevancyConfig>,
    /// Pre-stripped wire name for the relevancy `expected`
    /// binding. The workload author writes either
    /// `expected: ground_truth` (bare) or `expected: "{ground_truth}"`
    /// (text-template legacy form, the braces are stripped at
    /// wrap-time). Cycle-time reads go through
    /// `ctx.wires.get(&expected_wire_name)` for a typed,
    /// snapshot-free value — same canonical-scope contract the
    /// MetricsDispenser uses. None when no `relevancy:` block.
    expected_wire_name: Option<String>,
    /// SRD-109 Part 3 — pre-parsed projection for `actual:` when
    /// it names a result-binding path entry on this op (the
    /// `results:` interface delivery). The validator evaluates it
    /// directly from the op result: the `result` wrapper sits
    /// OUTSIDE validation in the cascade, so its wire write lands
    /// after this layer reads.
    actual_projection: Option<ActualProjection>,
    metrics: Arc<ValidationMetrics>,
    /// If true, assertion failures become ExecutionError::Op.
    strict: bool,
}

/// Pre-parsed `actual:` projection: the result-binding path plus
/// the interface-declared type (when the wire fills an
/// `abstract: results:` contract).
struct ActualProjection {
    segs: Vec<crate::wrappers::result::PathSeg>,
    target: Option<polydat::ast::PortType>,
}

/// Result of [`ValidatingDispenser::wrap`]: the (possibly-wrapped)
/// dispenser plus the live [`ValidationMetrics`] handle when a
/// `relevancy:` block was declared (`None` otherwise).
type WrappedDispenser = (Arc<dyn OpDispenser>, Option<Arc<ValidationMetrics>>);

impl ValidatingDispenser {
    /// Wrap a dispenser with validation, registering the relevancy
    /// `expected` binding into the supplied scope fixture.
    ///
    /// Returns the inner dispenser unchanged if neither `verify:`
    /// nor `relevancy:` are declared.
    pub fn wrap(
        inner: Arc<dyn OpDispenser>,
        template: &nmbrs_workload::model::ParsedOp,
        labels: &Labels,
        program: Option<&polydat::kernel::PolydatProgram>,
        fx: &mut crate::fixture::ScopeFixture,
    ) -> Result<WrappedDispenser, String> {
        // SRD-68 Push 5c-cleanup: validation wrapper does its own
        // construction-time resolution of `{name}` placeholders
        // in op.params against the dispenser's canonical kernel.
        // This replaces the legacy `resolve_placeholders_in_params_only`
        // bulk pass at the executor layer — each wrapper now
        // resolves against its own dispenser's canonical kernel
        // rather than a shared activity-layer parent. Per-cycle
        // binding names (e.g. `relevancy.expected = "{ground_truth}"`)
        // pass through unchanged; the wrapper registers them on
        // the fixture for cycle-time pulls below.
        let template_owned: nmbrs_workload::model::ParsedOp;
        let canonical_kernel = inner.canonical_kernel();
        let template = if let Some(canonical) = &canonical_kernel {
            let mut t = template.clone();
            crate::scope::resolve_placeholders_in_op_params(&mut t, canonical.as_ref())?;
            template_owned = t;
            &template_owned
        } else {
            template
        };
        let assertions = parse_assertions(template);
        // Bare wire-name forms in `k:` / `r:` resolve against the
        // canonical kernel at wrap time (one-shot, same as the
        // pre-existing `{k}` text-template substitution). The
        // canonical kernel implements `WireSource` directly per
        // SRD-68 Push 1; no per-cycle freshness because k / r are
        // phase-constants by contract.
        let wires_for_parse: Option<&dyn WireSource> = canonical_kernel
            .as_ref()
            .map(|k| k.as_ref() as &dyn WireSource);
        let relevancy = parse_relevancy(template, program, wires_for_parse)?;
        let strict = template
            .params
            .get("strict")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if assertions.is_empty() && relevancy.is_none() {
            return Ok((inner, None));
        }

        // Validate at wrap-time that the relevancy `expected`
        // binding exists on the op-template kernel — surfacing
        // missing-wire diagnostics before any cycle runs. The
        // returned PullHandle is unused (cycle-time reads go
        // through ctx.wires.get on the bare wire name), so we
        // only keep the name; same canonical-scope contract as
        // the post-SRD-68 MetricsDispenser.
        //
        // Tolerate either `expected: ground_truth` (bare, the
        // canonical post-refactor shape) or `expected: "{ground_truth}"`
        // (the legacy text-template form — braces strip).
        let expected_wire_name = match &relevancy {
            Some(cfg) => {
                let name = cfg
                    .expected_binding
                    .trim_matches(|c| c == '{' || c == '}')
                    .to_string();
                fx.register_pull(&name).map_err(|e| {
                    format!("op '{op}' relevancy.expected: {e}", op = template.name,)
                })?;
                Some(name)
            }
            None => None,
        };

        let metrics = Arc::new(match &relevancy {
            Some(config) => ValidationMetrics::new(labels, &config.functions, config.k, config.r),
            None => ValidationMetrics::assertions_only(),
        });

        // SRD-109 Part 3 — when `actual:` names a result-binding
        // path entry on this op, pre-parse the projection so the
        // per-cycle read evaluates it directly from the result
        // body (see the `actual_projection` field doc for why the
        // wire write can't be read from this layer).
        let actual_projection = match &relevancy {
            Some(cfg) => {
                let mut raw: Option<String> = None;
                if let Some(spec) = template.result.as_ref() {
                    spec.walk_fragments(|frag| {
                        if let nmbrs_workload::model::ResultFragment::Named { name, source } = frag
                            && name == cfg.actual_field
                        {
                            let s = source.trim();
                            if s != "count" && s != "ok" && !s.contains('(') {
                                raw = Some(s.to_string());
                            }
                        }
                    });
                }
                match raw {
                    Some(path) => {
                        let segs =
                            crate::wrappers::result::parse_path_expr(&path).map_err(|e| {
                                format!(
                                    "op '{op}' relevancy.actual '{field}': result \
                                 binding path: {e}",
                                    op = template.name,
                                    field = cfg.actual_field
                                )
                            })?;
                        let target = template
                            .abstract_interface
                            .as_ref()
                            .and_then(|i| i.results.get(&cfg.actual_field))
                            .and_then(|kw| polydat::ast::PortType::from_keyword(kw));
                        Some(ActualProjection { segs, target })
                    }
                    None => None,
                }
            }
            None => None,
        };

        let wrapper = Arc::new(Self {
            inner,
            assertions,
            relevancy,
            expected_wire_name,
            actual_projection,
            metrics: metrics.clone(),
            strict,
        });
        Ok((wrapper, Some(metrics)))
    }
}

impl WrappingDispenser for ValidatingDispenser {}

impl OpDispenser for ValidatingDispenser {
    /// Expose the wrapped dispenser so `describe()` can
    /// walk through this layer to reach the adapter
    /// (raw / prepared / batch). Without this override,
    /// the default trait method returns `None`, the
    /// describe walk stops here, and the error-context
    /// dump loses the CQL statement text.
    fn inner_dispenser(&self) -> Option<&dyn OpDispenser> {
        Some(self.inner.as_ref())
    }

    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a crate::fixture::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let result = self.inner.execute(cycle, ctx).await?;

            // Phase 1: Field assertions. Track failed predicates
            // so the strict-mode error can say which check failed
            // (especially helpful for body-level checks like
            // `min_rows: 1` where the user gets clear feedback that
            // the result was empty rather than a generic "validation
            // failed" message).
            let mut failed_assertions: Vec<String> = Vec::new();
            for assertion in &self.assertions {
                if !assertion.check(&result) {
                    failed_assertions.push(describe_assertion_failure(assertion, &result));
                }
            }
            let all_pass = failed_assertions.is_empty();

            // Phase 2: Relevancy metrics
            if let Some(config) = &self.relevancy {
                // SRD-109 Part 3 / SRD-70: `actual:` resolution
                // order — (1) a result-binding projection declared
                // on this op (evaluated directly from the result
                // body; the `result` wrapper's wire write lands
                // outside this layer), (2) a live wire (captures
                // and per-cycle bindings are fresh here), (3) the
                // legacy result-column walk for workloads that
                // predate the interface.
                let actual_ordered = if let Some(proj) = &self.actual_projection {
                    let projected = result.body.as_ref().and_then(|b| {
                        crate::wrappers::result::evaluate_path_value(
                            &b.to_json(),
                            &proj.segs,
                            proj.target,
                        )
                    });
                    projected
                        .as_ref()
                        .map(resolve_expected_from_value)
                        .unwrap_or_default()
                } else {
                    match ctx.wires.get(&config.actual_field) {
                        Some(v) if !matches!(v, polydat::ast::Value::None) => {
                            resolve_expected_from_value(&v)
                        }
                        _ => extract_actual_indices(&result, &config.actual_field),
                    }
                };
                // Single read path: ground truth comes from
                // `ctx.wires.get(name)` — a live, snapshot-free read
                // through the per-fiber op-template kernel. The wire
                // name was validated at wrap-time. Same canonical-scope
                // contract MetricsDispenser uses.
                let name = self.expected_wire_name.as_deref().expect(
                    "ValidatingDispenser invariant violated: relevancy is \
                     configured but expected_wire_name was not stored. \
                     Construct via ValidatingDispenser::wrap.",
                );
                let raw_value = ctx.wires.get(name);
                let expected_raw = raw_value
                    .as_ref()
                    .map(resolve_expected_from_value)
                    .unwrap_or_default();

                // Hard error if ground truth or actual results are empty
                if expected_raw.is_empty() {
                    let available: Vec<String> = ctx.wires.names().collect();
                    return Err(ExecutionError::Op(crate::adapter::AdapterError {
                        error_name: "relevancy_error".into(),
                        message: format!(
                            "relevancy: no ground truth for '{name}'. \
                             Available wires: {available:?}. \
                             Ensure the binding exists in the Polydat program.",
                        ),
                        retryable: false,
                    }));
                }
                if actual_ordered.is_empty() && result.body.is_some() {
                    // Log on first occurrence only
                    if self.metrics.passed() + self.metrics.failed() == 0 {
                        let indent = crate::scene_tree::running_phase_indent();
                        crate::observer::log(
                            crate::observer::LogLevel::Warn,
                            &format!(
                                "{indent}relevancy: no values extracted for field '{}' from result",
                                config.actual_field
                            ),
                        );
                        if let Some(body) = &result.body {
                            let preview =
                                serde_json::to_string(&body.to_json()).unwrap_or_default();
                            crate::observer::log(
                                crate::observer::LogLevel::Warn,
                                &format!(
                                    "{indent}  result preview: {}",
                                    &preview[..preview.len().min(300)]
                                ),
                            );
                        }
                    }
                }

                // k-recall@r contract: when the workload declares
                // `r:` in the relevancy block, it has asserted
                // that the database query was sized to retrieve
                // exactly that many results. Mismatch is a
                // correctness signal — the recall measurement is
                // only meaningful if the retrieval window matches
                // what the operator asked for. Fail the op rather
                // than silently producing a recall figure for an
                // off-by-one retrieval window.
                if let Some(r) = config.r
                    && actual_ordered.len() != r
                {
                    return Err(ExecutionError::Op(crate::adapter::AdapterError {
                        error_name: "relevancy_error".into(),
                        message: format!(
                            "relevancy: expected exactly r={r} results from \
                                 retrieval, got {} (k-recall@r contract). Either \
                                 size the query LIMIT to {r}, or remove the `r:` \
                                 declaration to fall back to first-k semantics.",
                            actual_ordered.len(),
                        ),
                        retryable: false,
                    }));
                }

                // k-recall@r semantics: the recall metric counts
                // how many of the *top-k* ground-truth items appear
                // anywhere in the *top-r* returned, divided by k.
                // Truncating `actual` to `k` (the pre-`r:` shape)
                // computes top-k ∩ top-k instead, which collapses
                // to ~K/R when the server post-filters and the
                // returned top-K rarely contains true neighbours —
                // that's the "stuck at 0.1" symptom for r=10×k.
                // When `r` is unset, fall back to k (the legacy
                // "first-k" behaviour).
                let r_window = config.r.unwrap_or(config.k);
                let expected_sorted = relevancy::truncate_and_sort(&expected_raw, config.k);
                let actual_sorted = relevancy::truncate_and_sort(&actual_ordered, r_window);

                for func in &config.functions {
                    let score =
                        func.compute(&expected_sorted, &actual_sorted, &actual_ordered, config.k);
                    self.metrics.record_relevancy(func.metric_name(), score);
                    // Generic observability point: a relevancy
                    // score has been computed. The trace fires
                    // for ANY relevancy config (recall, precision,
                    // F1, MRR, AP) — no knowledge of the
                    // workload's domain. Labels carry whatever
                    // dimensions the surrounding scope tree
                    // pushed (phase, profile, optimize_for, k,
                    // r), so `--trace=` routing/filtering is
                    // entirely config-driven.
                    if crate::observer::trace_enabled() {
                        let intersect =
                            crate::relevancy::intersection_count(&expected_sorted, &actual_sorted);
                        let stats_labels = self
                            .metrics
                            .relevancy_stats
                            .get(func.metric_name())
                            .map(|s| s.labels().clone())
                            .unwrap_or_default();
                        crate::observer::trace(
                            &stats_labels,
                            &format!(
                                "event=relevancy.score cycle={cycle} \
                                 fn={func} k={k} r={r} \
                                 gt_card={gt} actual_card={ac} \
                                 intersect={inter} score={score:.6}",
                                func = func.metric_name(),
                                k = config.k,
                                r = config.r.unwrap_or(config.k),
                                gt = expected_sorted.len(),
                                ac = actual_sorted.len(),
                                inter = intersect,
                            ),
                        );
                    }
                }
            }

            if all_pass {
                self.metrics
                    .validations_passed
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                self.metrics
                    .validations_failed
                    .fetch_add(1, Ordering::Relaxed);
                if self.strict {
                    return Err(ExecutionError::Op(crate::adapter::AdapterError {
                        error_name: "validation_failed".into(),
                        message: format!(
                            "result validation failed (strict mode): {}",
                            failed_assertions.join("; "),
                        ),
                        retryable: false,
                    }));
                }
            }

            Ok(result)
        })
    }
}

// =========================================================================
// YAML parsing
// =========================================================================

/// Render a one-line description of an assertion that failed
/// against `result`. Used by the strict-mode error message so
/// the workload author sees exactly which predicate fired,
/// what the resolved field value was (or why it couldn't be
/// resolved), AND the body excerpt — without all three the
/// operator has to manually re-run with logging cranked up
/// just to figure out what came back.
///
/// The body excerpt is suppressed when the body is `None` —
/// observed_field_repr's "<no body returned by op>" already
/// communicates that case, and a trailing `; body: <no body>`
/// duplicates the same signal.
fn describe_assertion_failure(assertion: &AssertionSpec, result: &OpResult) -> String {
    let body_tail = match &result.body {
        Some(_) => format!("; body: {}", body_excerpt(result)),
        None => String::new(),
    };
    let observed_repr = observed_field_repr(&assertion.field, result);
    match &assertion.predicate {
        AssertionPredicate::MinRows(n) => {
            let got = result.body.as_ref().map(|b| b.element_count()).unwrap_or(0);
            format!("min_rows: expected ≥{n}, got {got}{body_tail}")
        }
        AssertionPredicate::Eq(expected) => format!(
            "field '{}' eq '{}' failed (observed: {observed_repr}){body_tail}",
            assertion.field, expected
        ),
        AssertionPredicate::NotNull => format!(
            "field '{}' must not be null (observed: {observed_repr}){body_tail}",
            assertion.field
        ),
        AssertionPredicate::IsNull => format!(
            "field '{}' must be null (observed: {observed_repr}){body_tail}",
            assertion.field
        ),
        AssertionPredicate::Gte(t) => format!(
            "field '{}' >= {t} failed (observed: {observed_repr}){body_tail}",
            assertion.field
        ),
        AssertionPredicate::MalformedBound { key, raw } => format!(
            "field '{}': `{key}: {raw}` is not a number — a numeric bound \
                 must be a number (`{key}: 5`) or a string holding one \
                 (`{key}: \"5\"`). If that is a `{{placeholder}}`, it did not \
                 resolve.",
            assertion.field
        ),
        AssertionPredicate::Lte(t) => format!(
            "field '{}' <= {t} failed (observed: {observed_repr}){body_tail}",
            assertion.field
        ),
        AssertionPredicate::Contains(sub) => format!(
            "field '{}' contains '{}' failed (observed: {observed_repr}){body_tail}",
            assertion.field, sub
        ),
    }
}

/// Best-effort description of the observed value of a field.
/// Returns the JSON-extracted value (string-quoted if it is a
/// JSON string) when the field is addressable; otherwise a
/// diagnostic that surfaces *why* the field couldn't be read,
/// so the operator can tell whether to fix the field name or
/// the upstream response shape:
///
/// - `<no body returned by op — verify clause cannot read
///   fields>` when the op produced no body at all. Often a
///   sign the verify clause is structurally wrong for this
///   op kind (DDL, flush, anything that doesn't return rows).
/// - `<field absent; body keys: [a, b, c]>` when the body is
///   a JSON object but doesn't contain the requested field —
///   shows what IS there so the author can pick a real name.
/// - `<field absent; body is a JSON array of N elements>`
///   when the body is an array (`field:` addresses the first
///   element by convention; if the array is empty there's
///   nothing to address).
/// - `<not-json: {short repr}>` when the body parses to a
///   scalar or non-addressable JSON value (e.g. an HTML error
///   page or a plain-text response). Shows the first chars
///   so the operator can recognise the upstream's actual
///   payload.
/// - The extracted value otherwise, truncated for log
///   readability.
fn observed_field_repr(field: &str, result: &OpResult) -> String {
    let Some(body) = &result.body else {
        return "<no body returned by op — verify clause cannot read fields>".to_string();
    };
    let json = body.to_json();
    if let Some(v) = extract_field_from_json(&json, field) {
        let repr = match v {
            serde_json::Value::String(s) => format!("\"{s}\""),
            other => other.to_string(),
        };
        return truncate_for_message(&repr, 160);
    }
    // Field couldn't be resolved. The "why" guides the fix —
    // either the field name is wrong (the body has different
    // keys) or the response shape is wrong (not a structured
    // JSON object at all).
    match &json {
        serde_json::Value::Object(map) => {
            let keys: Vec<&str> = map.keys().map(|s| s.as_str()).collect();
            if keys.is_empty() {
                "<field absent; body is an empty JSON object>".to_string()
            } else {
                format!("<field absent; body keys: {keys:?}>")
            }
        }
        serde_json::Value::Array(arr) => {
            format!(
                "<field absent; body is a JSON array of {} element{}>",
                arr.len(),
                if arr.len() == 1 { "" } else { "s" },
            )
        }
        serde_json::Value::Null => "<field absent; body is JSON null>".to_string(),
        other => {
            let short = truncate_for_message(&other.to_string(), 80);
            format!("<not-json: {short}>")
        }
    }
}

/// A short excerpt of the response body, embedded in
/// assertion-failure messages so the operator can see what
/// actually came back. Caller is responsible for skipping the
/// excerpt when `result.body` is `None` — see
/// `describe_assertion_failure`.
fn body_excerpt(result: &OpResult) -> String {
    let Some(body) = &result.body else {
        return "<no body>".to_string();
    };
    truncate_for_message(&body.to_text(), 512)
}

/// Truncate a string to `max` chars with an ellipsis tail so
/// failure messages stay readable. Avoids splitting inside a
/// UTF-8 codepoint by using `char_indices`.
fn truncate_for_message(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// Parse `verify:` block from a ParsedOp's params.
///
/// Expected YAML structure:
/// ```yaml
/// verify:
///   - field: name
///     is: not_null
///   - field: balance
///     gte: 0
///   - field: data
///     eq: "expected_value"
///   - min_rows: 1     # body-level: result must have ≥1 row
/// ```
///
/// Most predicates target a named field within each result row
/// and require `field:`. The body-level `min_rows:` predicate is
/// the exception — it asserts on the result's row count rather
/// than any specific field, so it has no `field:` key.
/// A `gte:` / `lte:` threshold, from either a JSON number or a string
/// holding one.
///
/// YAML makes the string form easy to reach by accident — quoting, or a
/// `{placeholder}` that substitutes to text — and the two spellings plainly
/// mean the same bound, so both are accepted. What is NOT accepted is
/// anything else: `None` here becomes a
/// [`AssertionPredicate::MalformedBound`] that fails loudly, rather than the
/// old silent `0.0`.
fn numeric_bound(v: &serde_json::Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
}

fn parse_assertions(template: &nmbrs_workload::model::ParsedOp) -> Vec<AssertionSpec> {
    let Some(verify) = template.params.get("verify") else {
        return Vec::new();
    };

    let Some(items) = verify.as_array() else {
        return Vec::new();
    };

    let mut assertions = Vec::new();
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };

        // Body-level predicates first: `min_rows: N` doesn't take
        // a `field:` because it asserts on the body's row count.
        if let Some(v) = obj.get("min_rows") {
            let n = v.as_u64().unwrap_or(0);
            assertions.push(AssertionSpec {
                field: String::new(), // ignored for MinRows
                predicate: AssertionPredicate::MinRows(n),
            });
            continue;
        }

        let Some(field) = obj.get("field").and_then(|v| v.as_str()) else {
            continue;
        };

        let predicate = if let Some(v) = obj.get("eq") {
            AssertionPredicate::Eq(json_value_as_string(v))
        } else if let Some(v) = obj.get("gte") {
            match numeric_bound(v) {
                Some(t) => AssertionPredicate::Gte(t),
                None => AssertionPredicate::MalformedBound {
                    key: "gte".into(),
                    raw: json_value_as_string(v),
                },
            }
        } else if let Some(v) = obj.get("lte") {
            match numeric_bound(v) {
                Some(t) => AssertionPredicate::Lte(t),
                None => AssertionPredicate::MalformedBound {
                    key: "lte".into(),
                    raw: json_value_as_string(v),
                },
            }
        } else if let Some(v) = obj.get("contains") {
            AssertionPredicate::Contains(json_value_as_string(v))
        } else if let Some(v) = obj.get("is") {
            match v.as_str().unwrap_or("").to_lowercase().as_str() {
                "not_null" | "notnull" => AssertionPredicate::NotNull,
                "null" => AssertionPredicate::IsNull,
                _ => continue,
            }
        } else {
            continue;
        };

        assertions.push(AssertionSpec {
            field: field.to_string(),
            predicate,
        });
    }
    assertions
}

/// Allowed sub-keys under `relevancy:`. Anything outside this
/// vocabulary is a hard error — silent acceptance of typos
/// (e.g. `relevency:`) is exactly the failure mode SRD-15 §
/// "Strict mode" rules out.
const RELEVANCY_VOCAB: &[&str] = &["actual", "expected", "k", "r", "functions"];

/// Parse `relevancy:` block from a ParsedOp's params.
///
/// **Strict.** Required fields (`actual`, `expected`) missing →
/// error. Unknown sub-keys → error. Non-numeric `k`/`r` → error.
/// Surviving `{name}` placeholders → error (they should already
/// have been resolved by the SRD-16 single read path before this
/// function runs).
///
/// Expected YAML structure:
/// ```yaml
/// relevancy:
///   actual: key
///   expected: "{ground_truth}"
///   k: 10                       # or "{k}" — resolved beforehand
///   r: 64                       # optional
///   functions: [recall, …]
/// ```
fn parse_relevancy(
    template: &nmbrs_workload::model::ParsedOp,
    _program: Option<&polydat::kernel::PolydatProgram>,
    wires: Option<&dyn WireSource>,
) -> Result<Option<RelevancyConfig>, String> {
    let Some(rel) = template.params.get("relevancy") else {
        return Ok(None);
    };
    let obj = rel.as_object().ok_or_else(|| {
        format!(
            "op '{}': relevancy: expected a mapping, got {kind}",
            template.name,
            kind = match rel {
                serde_json::Value::Null => "null",
                serde_json::Value::Bool(_) => "boolean",
                serde_json::Value::Number(_) => "number",
                serde_json::Value::String(_) => "string",
                serde_json::Value::Array(_) => "array",
                _ => "unknown",
            },
        )
    })?;

    // Closed-vocabulary check.
    for k in obj.keys() {
        if !RELEVANCY_VOCAB.contains(&k.as_str()) {
            return Err(format!(
                "op '{op}' relevancy: unknown key '{k}'. Allowed: [{vocab}]",
                op = template.name,
                vocab = RELEVANCY_VOCAB.join(", "),
            ));
        }
    }

    let actual_field = obj
        .get("actual")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            format!(
                "op '{}' relevancy: missing required field 'actual' (string column name)",
                template.name,
            )
        })?
        .to_string();
    let expected_binding = obj
        .get("expected")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            format!(
                "op '{}' relevancy: missing required field 'expected' (binding reference)",
                template.name,
            )
        })?
        .to_string();

    let k_label = format!("op '{}' relevancy.k", template.name);
    let k = parse_count_param(obj.get("k"), &k_label, wires)?.ok_or_else(|| {
        format!(
            "op '{}' relevancy: missing required field 'k' (integer)",
            template.name,
        )
    })? as usize;

    let r_label = format!("op '{}' relevancy.r", template.name);
    let r: Option<usize> = parse_count_param(obj.get("r"), &r_label, wires)?.map(|n| n as usize);

    if let Some(rv) = r
        && rv < k
    {
        return Err(format!(
            "op '{op}' relevancy: r={rv} is smaller than k={k}; \
                 the k-recall@r contract requires r >= k",
            op = template.name,
        ));
    }

    let functions: Vec<RelevancyFn> = match obj.get("functions") {
        None => vec![RelevancyFn::Recall],
        Some(serde_json::Value::Array(arr)) => {
            let mut out: Vec<RelevancyFn> = Vec::new();
            for (i, v) in arr.iter().enumerate() {
                let name = v.as_str().ok_or_else(|| {
                    format!(
                        "op '{op}' relevancy.functions[{i}]: expected a string, got {kind}",
                        op = template.name,
                        kind = match v {
                            serde_json::Value::Null => "null",
                            serde_json::Value::Bool(_) => "boolean",
                            serde_json::Value::Number(_) => "number",
                            serde_json::Value::Array(_) => "array",
                            serde_json::Value::Object(_) => "object",
                            _ => "unknown",
                        },
                    )
                })?;
                let func = RelevancyFn::parse(name).ok_or_else(|| {
                    format!(
                        "op '{op}' relevancy.functions[{i}]: unknown function '{name}'",
                        op = template.name,
                    )
                })?;
                out.push(func);
            }
            if out.is_empty() {
                return Err(format!(
                    "op '{op}' relevancy.functions: empty list — declare at least one \
                     function or remove the field to default to [recall]",
                    op = template.name,
                ));
            }
            out
        }
        Some(_) => {
            return Err(format!(
                "op '{op}' relevancy.functions: expected an array of strings",
                op = template.name,
            ));
        }
    };

    Ok(Some(RelevancyConfig {
        actual_field,
        expected_binding,
        k,
        r,
        functions,
    }))
}

/// Parse a YAML count parameter (`k:` or `r:`) — strict.
/// Accepts only a JSON integer or a literal numeric string.
/// `{name}` placeholders are *not* resolved here: the SRD-16
/// single read path means every placeholder must already have
/// been resolved by
/// [`crate::scope::resolve_placeholders_in_op_params`] (called
/// from `ValidatingDispenser::wrap` against the dispenser's own
/// canonical kernel — SRD-68 Push 5c-cleanup). Surviving `{…}`
/// shapes are a workload bug and surface as `Err`. The caller
/// decides whether the parameter is required or optional —
/// `None` here means "absent," not "unresolvable."
fn parse_count_param(
    val: Option<&serde_json::Value>,
    field_label: &str,
    wires: Option<&dyn WireSource>,
) -> Result<Option<u64>, String> {
    let Some(v) = val else {
        return Ok(None);
    };
    if let Some(n) = v.as_u64() {
        return Ok(Some(n));
    }
    let s = match v.as_str() {
        Some(s) => s,
        None => {
            return Err(format!(
                "{field_label}: expected an integer or numeric string, got {kind}",
                kind = match v {
                    serde_json::Value::Null => "null",
                    serde_json::Value::Bool(_) => "boolean",
                    serde_json::Value::Array(_) => "array",
                    serde_json::Value::Object(_) => "object",
                    _ => "unsupported value",
                },
            ));
        }
    };
    let trimmed = s.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Err(format!(
            "{field_label}: '{trimmed}' was not resolved before parameter parsing — \
             this is a placeholder-resolution bug, not a config-time issue. The \
             single-read-path resolver should have substituted it from the kernel."
        ));
    }
    // Bare wire-name form: post-SRD-68 follow-up. When `k:` or `r:`
    // names a bare identifier and the canonical kernel knows it,
    // read the value at wrap time. Same one-shot evaluation as the
    // legacy `{k}` text-template form, but without the placeholder
    // wrapper. Numeric strings ("100") fall through to the parse
    // below — they're literals, not wire references.
    if let Some(wires) = wires
        && is_bare_ident(trimmed)
        && let Some(value) = wires.get(trimmed)
    {
        return value_to_u64_for_count(value)
            .ok_or_else(|| {
                format!(
                    "{field_label}: wire '{trimmed}' resolved but its value is not \
             coercible to a non-negative integer"
                )
            })
            .map(Some);
    }
    match trimmed.parse::<u64>() {
        Ok(n) => Ok(Some(n)),
        Err(_) => Err(format!(
            "{field_label}: '{trimmed}' is not a valid non-negative integer \
             (and not declared as a wire name on the op-template kernel)"
        )),
    }
}

/// Predicate for a bare Polydat identifier (single ident-shaped token).
/// Inlined locally to avoid pulling the `crate::wires::is_bare_ident`
/// pub-but-unexported helper into this file's surface.
fn is_bare_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Coerce a kernel `Value` to a non-negative integer suitable for
/// `k:` / `r:` count fields. U64 / F64 (non-negative) accepted;
/// other variants signal a type mismatch the caller surfaces.
fn value_to_u64_for_count(value: polydat::ast::Value) -> Option<u64> {
    use polydat::ast::Value;
    match value {
        Value::U64(n) => Some(n),
        Value::F64(f) if f.is_finite() && f >= 0.0 => Some(f as u64),
        Value::Bool(true) => Some(1),
        Value::Bool(false) => Some(0),
        _ => None,
    }
}

// =========================================================================
// Result extraction
// =========================================================================

/// Extract integer indices from a result body for relevancy comparison.
///
/// Tries adapter-native downcast first, then falls back to JSON extraction.
fn extract_actual_indices(result: &OpResult, field: &str) -> Vec<i64> {
    let Some(body) = &result.body else {
        return Vec::new();
    };
    extract_indices_from_json(&body.to_json(), field)
}

/// Extract integer values for a named field from JSON result structure.
fn extract_indices_from_json(json: &serde_json::Value, field: &str) -> Vec<i64> {
    match json {
        serde_json::Value::Array(rows) => rows
            .iter()
            .filter_map(|row| json_field_as_i64(row.get(field)?))
            .collect(),
        serde_json::Value::Object(obj) => {
            if let Some(rows) = obj.get("rows") {
                return extract_indices_from_json(rows, field);
            }
            obj.get(field)
                .and_then(json_field_as_i64)
                .into_iter()
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Coerce a JSON value to i64: native integer, or parse from string.
fn json_field_as_i64(v: &serde_json::Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.parse().ok())
}

/// Extract integer ground-truth indices from a typed `Value`.
///
/// Single read path: at cycle time, [`ValidatingDispenser::execute`]
/// reads the value via `ctx.wires.get(name)` (a live read through
/// the per-fiber op-template kernel — same canonical-scope contract
/// MetricsDispenser uses), then hands it here for type-aware
/// extraction.
///
/// Fast path for the typed-array vector data path (SRD 53
/// §"Native vector PortType"): when the binding produces
/// `Value::VecI32` / `Value::VecF32` (the shape
/// `neighbor_indices_at` and friends emit), the slice is read
/// directly with no string round-trip. String fallback covers
/// legacy bindings that emit `Value::Str("[1, 5, 12, ...]")` or
/// `Value::Str("1,5,12,...")`.
fn resolve_expected_from_value(value: &polydat::ast::Value) -> Vec<i64> {
    match value {
        // Typed-slice fast paths — zero-copy read, no parse.
        polydat::ast::Value::VecI32(slice) => slice.as_slice().iter().map(|&x| x as i64).collect(),
        polydat::ast::Value::VecI64(slice) => slice.as_slice().to_vec(),
        polydat::ast::Value::VecF32(slice) => {
            // Vector ground-truth indices are integer-valued
            // by domain. Truncating fractional parts is the
            // correct semantic; anything else would mean the
            // dataset's index column is mis-typed at the source.
            slice.as_slice().iter().map(|&x| x as i64).collect()
        }
        polydat::ast::Value::VecF64(slice) => slice.as_slice().iter().map(|&x| x as i64).collect(),
        // SRD-70 projection landed as structural JSON (mixed or
        // string-typed columns): integer-valued leaves extract,
        // numeric strings parse, anything else drops.
        polydat::ast::Value::Json(j) => match &**j {
            serde_json::Value::Array(elems) => elems.iter().filter_map(json_field_as_i64).collect(),
            other => json_field_as_i64(other).into_iter().collect(),
        },
        polydat::ast::Value::Str(s) => parse_int_array(s),
        polydat::ast::Value::U64(v) => vec![*v as i64],
        _ => {
            // Display-string fallback for anything else.
            let s = value.to_display_string();
            parse_int_array(&s)
        }
    }
}

/// Parse a string containing integers into a Vec<i64>.
///
/// Handles formats: `[1, 5, 12]`, `1,5,12`, `1 5 12`.
fn parse_int_array(s: &str) -> Vec<i64> {
    let trimmed = s.trim().trim_start_matches('[').trim_end_matches(']');
    trimmed
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect()
}

/// Extract a field from JSON by name, checking both top-level and row arrays.
fn extract_field_from_json<'a>(
    json: &'a serde_json::Value,
    field: &str,
) -> Option<&'a serde_json::Value> {
    match json {
        serde_json::Value::Object(obj) => obj.get(field).or_else(|| {
            obj.get("rows")
                .and_then(|r| r.as_array())
                .and_then(|rows| rows.first())
                .and_then(|row| row.get(field))
        }),
        serde_json::Value::Array(rows) => rows.first().and_then(|row| row.get(field)),
        _ => None,
    }
}

/// Convert a JSON value to its string representation for comparison.
fn json_value_as_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::ResultBody;
    use std::any::Any;

    #[derive(Debug)]
    struct JsonBody(serde_json::Value);
    impl ResultBody for JsonBody {
        fn to_json(&self) -> serde_json::Value {
            self.0.clone()
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn parse_int_array_bracket_format() {
        assert_eq!(parse_int_array("[1, 5, 12, 23]"), vec![1, 5, 12, 23]);
    }

    /// SRD-32a unification drift-guard: `CORE_OP_PARAMS` is the
    /// NON-wrapper core vocabulary. Wrapper fields are accepted by the
    /// op closed-vocab guard via the registry's `owns_field`, so a
    /// wrapper field appearing in `CORE_OP_PARAMS` is duplication that
    /// reintroduces the very two-lists-to-maintain trap this unification
    /// removed. Fail loudly if one creeps back in.
    #[test]
    fn core_op_params_disjoint_from_owned_fields() {
        let registry = crate::wrapper_registry::WrapperRegistry::from_inventory();
        let owned = registry.all_owned_fields();
        let dupes: Vec<&str> = CORE_OP_PARAMS
            .iter()
            .copied()
            .filter(|p| owned.contains(p))
            .collect();
        assert!(
            dupes.is_empty(),
            "these CORE_OP_PARAMS are already wrapper-owned (remove them — \
             the guard accepts them via WrapperRegistry::owns_field): {dupes:?}",
        );
    }

    /// The type-safe replacement for the CLI-coincidence hole: a
    /// wrapper field that is NEITHER a CLI param NOR in CORE_OP_PARAMS
    /// must still be recognized, purely because a wrapper declares it.
    /// `readout` is exactly such a field (opt-in op status, never CLI-
    /// settable), so it is the live regression witness.
    #[test]
    fn wrapper_field_accepted_without_cli_or_core_membership() {
        let registry = crate::wrapper_registry::WrapperRegistry::from_inventory();
        assert!(
            registry.owns_field("readout"),
            "readout must be registry-owned"
        );
        assert!(
            registry.owns_field("errors"),
            "errors must be registry-owned (was riding the CLI hatch)"
        );
        assert!(
            registry.owns_field("tries"),
            "tries must be registry-owned (was riding the CLI hatch)"
        );
        assert!(
            !CORE_OP_PARAMS.contains(&"readout"),
            "readout should NOT be in CORE_OP_PARAMS — it's wrapper-owned"
        );
    }

    #[test]
    fn parse_int_array_comma_format() {
        assert_eq!(parse_int_array("1,5,12,23"), vec![1, 5, 12, 23]);
    }

    #[test]
    fn parse_int_array_space_format() {
        assert_eq!(parse_int_array("1 5 12 23"), vec![1, 5, 12, 23]);
    }

    #[test]
    fn parse_int_array_empty() {
        assert_eq!(parse_int_array("[]"), Vec::<i64>::new());
        assert_eq!(parse_int_array(""), Vec::<i64>::new());
    }

    #[test]
    fn extract_indices_from_json_array() {
        let json = serde_json::json!([
            {"key": 5, "distance": 0.1},
            {"key": 12, "distance": 0.2},
            {"key": 3, "distance": 0.3},
        ]);
        assert_eq!(extract_indices_from_json(&json, "key"), vec![5, 12, 3]);
    }

    #[test]
    fn extract_indices_from_json_rows_wrapper() {
        let json = serde_json::json!({
            "rows": [
                {"key": 5},
                {"key": 12},
            ]
        });
        assert_eq!(extract_indices_from_json(&json, "key"), vec![5, 12]);
    }

    #[test]
    fn assertion_not_null() {
        let result = OpResult {
            body: Some(Box::new(JsonBody(serde_json::json!({"name": "alice"})))),
            skipped: false,
        };
        let spec = AssertionSpec {
            field: "name".into(),
            predicate: AssertionPredicate::NotNull,
        };
        assert!(spec.check(&result));

        let spec_missing = AssertionSpec {
            field: "age".into(),
            predicate: AssertionPredicate::NotNull,
        };
        assert!(!spec_missing.check(&result));
    }

    #[test]
    fn assertion_eq() {
        let result = OpResult {
            body: Some(Box::new(JsonBody(serde_json::json!({"status": "ok"})))),
            skipped: false,
        };
        let spec = AssertionSpec {
            field: "status".into(),
            predicate: AssertionPredicate::Eq("ok".into()),
        };
        assert!(spec.check(&result));

        let spec_fail = AssertionSpec {
            field: "status".into(),
            predicate: AssertionPredicate::Eq("error".into()),
        };
        assert!(!spec_fail.check(&result));
    }

    #[test]
    fn assertion_gte() {
        let result = OpResult {
            body: Some(Box::new(JsonBody(serde_json::json!({"balance": 42.5})))),
            skipped: false,
        };
        let spec = AssertionSpec {
            field: "balance".into(),
            predicate: AssertionPredicate::Gte(0.0),
        };
        assert!(spec.check(&result));

        let spec_fail = AssertionSpec {
            field: "balance".into(),
            predicate: AssertionPredicate::Gte(100.0),
        };
        assert!(!spec_fail.check(&result));
    }

    #[test]
    fn assertion_no_body() {
        let result = OpResult {
            body: None,
            skipped: false,
        };
        let spec = AssertionSpec {
            field: "anything".into(),
            predicate: AssertionPredicate::IsNull,
        };
        assert!(spec.check(&result));

        let spec_not_null = AssertionSpec {
            field: "anything".into(),
            predicate: AssertionPredicate::NotNull,
        };
        assert!(!spec_not_null.check(&result));
    }

    /// Body whose `element_count()` reflects the number of items
    /// in a JSON array — exercises body-level predicates like
    /// `MinRows` that read from `element_count` rather than
    /// inspecting fields.
    #[derive(Debug)]
    struct CountedBody {
        rows: Vec<serde_json::Value>,
    }
    impl ResultBody for CountedBody {
        fn to_json(&self) -> serde_json::Value {
            serde_json::Value::Array(self.rows.clone())
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn element_count(&self) -> u64 {
            self.rows.len() as u64
        }
    }

    #[test]
    fn assertion_min_rows_passes_when_threshold_met() {
        let result = OpResult {
            body: Some(Box::new(CountedBody {
                rows: vec![
                    serde_json::json!({"index_name": "vec_idx"}),
                    serde_json::json!({"index_name": "meta_idx"}),
                ],
            })),
            skipped: false,
        };
        let spec = AssertionSpec {
            field: String::new(),
            predicate: AssertionPredicate::MinRows(1),
        };
        assert!(spec.check(&result));

        let spec_two = AssertionSpec {
            field: String::new(),
            predicate: AssertionPredicate::MinRows(2),
        };
        assert!(spec_two.check(&result));
    }

    #[test]
    fn assertion_min_rows_fails_when_below_threshold() {
        // Empty array: element_count = 0, MinRows(1) must fail.
        let result = OpResult {
            body: Some(Box::new(CountedBody { rows: Vec::new() })),
            skipped: false,
        };
        let spec = AssertionSpec {
            field: String::new(),
            predicate: AssertionPredicate::MinRows(1),
        };
        assert!(!spec.check(&result));

        // No body at all: element_count defaults to 0 → MinRows(1) fails.
        let result_none = OpResult {
            body: None,
            skipped: false,
        };
        assert!(!spec.check(&result_none));
    }

    #[test]
    fn parse_assertions_min_rows_from_yaml() {
        // SRD-40b-shaped failsafe verify form:
        //   verify:
        //     - min_rows: 1
        let mut template = nmbrs_workload::model::ParsedOp::simple("await", "test");
        template.params.insert(
            "verify".into(),
            serde_json::json!([
                {"min_rows": 1},
            ]),
        );
        let assertions = parse_assertions(&template);
        assert_eq!(assertions.len(), 1);
        match &assertions[0].predicate {
            AssertionPredicate::MinRows(n) => assert_eq!(*n, 1),
            other => panic!("expected MinRows(1), got {other:?}"),
        }
    }

    #[test]
    fn eq_failure_includes_body_and_distinguishes_absent_vs_not_json() {
        // JSON body that has fields but lacks the one we asked
        // for: observed reads `<field absent; …>` and lists the
        // keys that ARE present so the operator can pick a real
        // field name without re-running the workload.
        let result = OpResult {
            body: Some(Box::new(JsonBody(serde_json::json!({
                "value": null, "request": {"type": "exec"}
            })))),
            skipped: false,
        };
        let spec = AssertionSpec {
            field: "status".into(),
            predicate: AssertionPredicate::Eq("200".into()),
        };
        let msg = describe_assertion_failure(&spec, &result);
        assert!(
            msg.contains("field absent"),
            "json-without-field should mark observed as absent, got: {msg}"
        );
        assert!(
            msg.contains("body keys"),
            "absent message should enumerate present keys, got: {msg}"
        );
        assert!(
            msg.contains("\"value\"") && msg.contains("\"request\""),
            "key list should include both present keys, got: {msg}"
        );
        assert!(msg.contains("body: "), "body excerpt missing: {msg}");
        assert!(
            msg.contains("\"request\""),
            "body excerpt should echo the actual JSON: {msg}"
        );

        // Plain-text body: observed reads `<not-json: …>` with a
        // short preview so the operator can recognise the actual
        // payload (HTML error page, plain-text response, etc.).
        #[derive(Debug)]
        struct PlainBody(String);
        impl ResultBody for PlainBody {
            fn to_json(&self) -> serde_json::Value {
                serde_json::Value::String(self.0.clone())
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
            fn to_text(&self) -> String {
                self.0.clone()
            }
        }
        let text_result = OpResult {
            body: Some(Box::new(PlainBody(
                "<html><body>404 Not Found</body></html>".into(),
            ))),
            skipped: false,
        };
        let msg2 = describe_assertion_failure(&spec, &text_result);
        assert!(
            msg2.contains("not-json"),
            "text body should mark observed as not-json, got: {msg2}"
        );
        assert!(
            msg2.contains("404 Not Found"),
            "body excerpt should include the text: {msg2}"
        );
    }

    /// When the op produced no body at all, the message must
    /// communicate that clearly (not just '<no body>' twice),
    /// AND must NOT duplicate the same '<no body>' phrase in
    /// both the observed-field slot and the body excerpt.
    #[test]
    fn eq_failure_with_no_body_explains_situation() {
        let result = OpResult {
            body: None,
            skipped: false,
        };
        let spec = AssertionSpec {
            field: "status".into(),
            predicate: AssertionPredicate::Eq("200".into()),
        };
        let msg = describe_assertion_failure(&spec, &result);
        assert!(
            msg.contains("no body returned by op"),
            "no-body case should explain why the field can't be read, got: {msg}"
        );
        assert!(
            !msg.contains("body: "),
            "no-body case should suppress the redundant body excerpt, got: {msg}"
        );
    }

    #[test]
    fn min_rows_failure_describes_actual_vs_expected() {
        // Strict-mode error renderer should name the predicate
        // and the actual count so the user sees "expected ≥1, got 0"
        // rather than a generic "validation failed".
        let result = OpResult {
            body: Some(Box::new(CountedBody { rows: Vec::new() })),
            skipped: false,
        };
        let spec = AssertionSpec {
            field: String::new(),
            predicate: AssertionPredicate::MinRows(1),
        };
        let msg = describe_assertion_failure(&spec, &result);
        assert!(msg.contains("min_rows"), "got: {msg}");
        assert!(msg.contains("≥1"), "got: {msg}");
        assert!(msg.contains("got 0"), "got: {msg}");
    }

    #[test]
    fn validation_metrics_record_relevancy() {
        let labels = Labels::of("activity", "test");
        let metrics = ValidationMetrics::new(
            &labels,
            &[RelevancyFn::Recall, RelevancyFn::Precision],
            10,
            Some(20),
        );
        // Family names are bare function names; `k` and
        // `r` ride on the F64Stats's labels so consumers
        // can query e.g. `recall{k="10",r="20"}`.
        assert!(metrics.relevancy_stats.contains_key("recall"));
        assert!(metrics.relevancy_stats.contains_key("precision"));
        assert!(!metrics.relevancy_stats.contains_key("f1"));

        // The F64Stats labels carry k and r.
        let recall_labels = metrics.relevancy_stats["recall"].labels();
        assert_eq!(recall_labels.get("k"), Some("10"));
        assert_eq!(recall_labels.get("r"), Some("20"));

        metrics.record_relevancy("recall", 0.85);
        metrics.record_relevancy("recall", 0.90);
        let snap = metrics.relevancy_stats["recall"].snapshot();
        assert_eq!(snap.len(), 2);
    }

    #[test]
    fn validation_metrics_r_defaults_to_k() {
        let labels = Labels::of("activity", "test");
        // No `r:` in the relevancy config → the metric's
        // `r` label equals `k` (legacy first-k semantics).
        let metrics = ValidationMetrics::new(&labels, &[RelevancyFn::Recall], 100, None);
        let l = metrics.relevancy_stats["recall"].labels();
        assert_eq!(l.get("k"), Some("100"));
        assert_eq!(l.get("r"), Some("100"));
    }

    #[test]
    fn resolve_expected_string_array_form() {
        let v = polydat::ast::Value::Str("[1, 5, 12, 23]".into());
        assert_eq!(resolve_expected_from_value(&v), vec![1, 5, 12, 23]);
    }

    #[test]
    fn resolve_expected_string_csv_form() {
        let v = polydat::ast::Value::Str("1,5,12".into());
        assert_eq!(resolve_expected_from_value(&v), vec![1, 5, 12]);
    }

    #[test]
    fn resolve_expected_native_veci32_fast_path() {
        // The fast path the dataset accessors emit
        // (`neighbor_indices_at` etc. → Value::VecI32). No
        // string round-trip happens; the slice is read directly.
        use polydat::ast::{SliceArc, Value};
        let slice = SliceArc::<i32>::from_vec(vec![1, 5, 12, 23, 100]);
        let v = Value::VecI32(slice);
        assert_eq!(resolve_expected_from_value(&v), vec![1, 5, 12, 23, 100]);
    }

    #[test]
    fn resolve_expected_native_vecf32_fast_path() {
        // VecF32 ground truth (rare but legal — some datasets
        // store ranks as floats). Truncates to i64.
        use polydat::ast::{SliceArc, Value};
        let slice = SliceArc::<f32>::from_vec(vec![1.0, 2.0, 3.0]);
        let v = Value::VecF32(slice);
        assert_eq!(resolve_expected_from_value(&v), vec![1, 2, 3]);
    }

    #[test]
    fn parse_relevancy_from_params() {
        let mut template = nmbrs_workload::model::ParsedOp::simple("test", "SELECT key FROM t");
        template.params.insert(
            "relevancy".into(),
            serde_json::json!({
                "actual": "key",
                "expected": "{ground_truth}",
                "k": 10,
                "functions": ["recall", "precision", "f1"]
            }),
        );
        let config = parse_relevancy(&template, None, None).unwrap().unwrap();
        assert_eq!(config.actual_field, "key");
        assert_eq!(config.expected_binding, "{ground_truth}");
        assert_eq!(config.k, 10);
        assert!(config.r.is_none(), "r should default to None when absent");
        assert_eq!(config.functions.len(), 3);
        assert_eq!(config.functions[0], RelevancyFn::Recall);
        assert_eq!(config.functions[1], RelevancyFn::Precision);
        assert_eq!(config.functions[2], RelevancyFn::F1);
    }

    #[test]
    fn parse_relevancy_with_r_for_k_recall_at_r() {
        // 10-recall@100: retrieve 100, score against the first
        // 10. The `r` field is parsed alongside `k` from the
        // same numeric/`{name}` shapes.
        let mut template = nmbrs_workload::model::ParsedOp::simple("test", "SELECT key FROM t");
        template.params.insert(
            "relevancy".into(),
            serde_json::json!({
                "actual": "key",
                "expected": "{ground_truth}",
                "k": 10,
                "r": 100,
                "functions": ["recall"],
            }),
        );
        let config = parse_relevancy(&template, None, None).unwrap().unwrap();
        assert_eq!(config.k, 10);
        assert_eq!(config.r, Some(100));
    }

    #[test]
    fn parse_relevancy_r_accepts_string_form() {
        let mut template = nmbrs_workload::model::ParsedOp::simple("test", "SELECT key FROM t");
        template.params.insert(
            "relevancy".into(),
            serde_json::json!({
                "actual": "key",
                "expected": "{ground_truth}",
                "k": "10",
                "r": "100",
            }),
        );
        let config = parse_relevancy(&template, None, None).unwrap().unwrap();
        assert_eq!(config.k, 10);
        assert_eq!(config.r, Some(100));
    }

    #[test]
    fn parse_relevancy_missing() {
        let template = nmbrs_workload::model::ParsedOp::simple("test", "INSERT");
        assert!(parse_relevancy(&template, None, None).unwrap().is_none());
    }

    #[test]
    fn parse_relevancy_k_and_r_accept_bare_wire_names() {
        // Post-SRD-68 follow-up: `k: k` (bare wire-name) resolves
        // against the canonical kernel at wrap time. Same one-shot
        // evaluation as the `{k}` text-template form but without
        // the placeholder braces.
        use polydat::dsl::compile::compile_polydat_interpreter;
        let kernel = compile_polydat_interpreter(
            "input cycle: u64\n\
             const k := 10\n\
             const limit := 100\n",
        )
        .expect("compile_polydat_interpreter wires");
        let wires: &dyn WireSource = &kernel;

        let mut template = nmbrs_workload::model::ParsedOp::simple("test", "SELECT key FROM t");
        template.params.insert(
            "relevancy".into(),
            serde_json::json!({
                "actual": "key",
                "expected": "{ground_truth}",
                "k": "k",            // bare wire-name
                "r": "limit",        // bare wire-name
                "functions": ["recall"],
            }),
        );
        let config = parse_relevancy(&template, None, Some(wires))
            .unwrap()
            .unwrap();
        assert_eq!(config.k, 10, "bare `k:` resolved through wires");
        assert_eq!(config.r, Some(100), "bare `r:` resolved through wires");
    }

    #[test]
    fn parse_relevancy_bare_name_falls_through_to_int_parse_when_no_kernel() {
        // Without a canonical kernel (no wires available), a bare
        // identifier in `k:` errors clearly — it can't be a wire
        // reference and isn't a valid integer either.
        let mut template = nmbrs_workload::model::ParsedOp::simple("test", "SELECT key FROM t");
        template.params.insert(
            "relevancy".into(),
            serde_json::json!({
                "actual": "key",
                "expected": "{ground_truth}",
                "k": "k",
                "functions": ["recall"],
            }),
        );
        let err = parse_relevancy(&template, None, None).unwrap_err();
        assert!(
            err.contains("'k' is not a valid non-negative integer"),
            "diagnostic should describe the parse failure: {err}"
        );
    }

    /// A succeeding op that returns NO body must not be failed by a value
    /// predicate. `on_timeout: accept` (the server is still working; the poll
    /// layer observes it) and HTTP 204 both land here, and a
    /// `field: status, eq: 200` clause written for the normal response was
    /// failing them — it cost a live benchmark run at tier 6.
    #[test]
    fn value_predicates_are_vacuous_without_a_body() {
        let result = OpResult {
            body: None,
            ..Default::default()
        };
        for predicate in [
            AssertionPredicate::Eq("200".into()),
            AssertionPredicate::Lte(5.0),
            AssertionPredicate::Gte(1.0),
            AssertionPredicate::Contains("ok".into()),
            AssertionPredicate::IsNull,
        ] {
            let spec = AssertionSpec {
                field: "status".into(),
                predicate,
            };
            assert!(
                spec.check(&result),
                "a value predicate has nothing to contradict it: {:?}",
                spec.predicate
            );
        }
    }

    /// Presence predicates are the explicit "a body is required" spelling, so
    /// they must still fail when none arrives — otherwise there would be no
    /// way to demand one.
    #[test]
    fn presence_predicates_still_fail_without_a_body() {
        let result = OpResult {
            body: None,
            ..Default::default()
        };
        let not_null = AssertionSpec {
            field: "status".into(),
            predicate: AssertionPredicate::NotNull,
        };
        assert!(
            !not_null.check(&result),
            "`is: not_null` is how an author demands the field exist"
        );
        let min_rows = AssertionSpec {
            field: String::new(),
            predicate: AssertionPredicate::MinRows(1),
        };
        assert!(
            !min_rows.check(&result),
            "`min_rows: 1` is how an author demands a non-empty result"
        );
    }

    /// A malformed bound can never pass, body or no body — otherwise a typo
    /// would turn into a silently-skipped check on exactly the ops that
    /// return nothing.
    #[test]
    fn malformed_bounds_fail_even_without_a_body() {
        let result = OpResult {
            body: None,
            ..Default::default()
        };
        let spec = AssertionSpec {
            field: "value".into(),
            predicate: AssertionPredicate::MalformedBound {
                key: "lte".into(),
                raw: "{unresolved}".into(),
            },
        };
        assert!(!spec.check(&result));
    }

    /// A quoted numeric bound means the same as an unquoted one. YAML makes
    /// the string form easy to reach — quoting, or a `{placeholder}` that
    /// substitutes to text — and it used to silently become `<= 0`, failing
    /// good data against a threshold nobody wrote. Cost a live benchmark run.
    #[test]
    fn numeric_bounds_accept_quoted_numbers() {
        let mut template = nmbrs_workload::model::ParsedOp::simple("t", "noop");
        template.params.insert(
            "verify".into(),
            serde_json::json!([
                {"field": "value", "lte": "5"},
                {"field": "value", "gte": " 2 "},
                {"field": "other", "lte": 7},
            ]),
        );
        let a = parse_assertions(&template);
        assert!(
            matches!(a[0].predicate, AssertionPredicate::Lte(t) if t == 5.0),
            "quoted lte must parse: {:?}",
            a[0].predicate
        );
        assert!(
            matches!(a[1].predicate, AssertionPredicate::Gte(t) if t == 2.0),
            "surrounding whitespace is not a malformed bound: {:?}",
            a[1].predicate
        );
        assert!(
            matches!(a[2].predicate, AssertionPredicate::Lte(t) if t == 7.0),
            "the unquoted form is unchanged: {:?}",
            a[2].predicate
        );
    }

    /// A bound that is not a number at all must FAIL LOUDLY and name itself —
    /// never default to zero, which reads as a real threshold in the failure
    /// message and sends the reader hunting for a data problem that does not
    /// exist.
    #[test]
    fn non_numeric_bounds_are_malformed_not_zero() {
        let mut template = nmbrs_workload::model::ParsedOp::simple("t", "noop");
        template.params.insert(
            "verify".into(),
            serde_json::json!([
                {"field": "value", "lte": "{unresolved}"},
                {"field": "value", "gte": "abc"},
            ]),
        );
        let a = parse_assertions(&template);
        for spec in &a {
            match &spec.predicate {
                AssertionPredicate::MalformedBound { raw, .. } => {
                    assert!(
                        !raw.is_empty(),
                        "the offending text is carried for the message"
                    );
                }
                other => panic!("expected MalformedBound, got {other:?}"),
            }
        }
        // And the rendered failure names the malformed bound rather than
        // quoting a threshold the author never wrote.
        let msg = format!("{:?}", a[0].predicate);
        assert!(
            msg.contains("MalformedBound"),
            "the predicate stays malformed all the way to reporting: {msg}"
        );
    }

    #[test]
    fn parse_assertions_from_params() {
        let mut template = nmbrs_workload::model::ParsedOp::simple("test", "SELECT");
        template.params.insert(
            "verify".into(),
            serde_json::json!([
                {"field": "name", "is": "not_null"},
                {"field": "balance", "gte": 0},
                {"field": "status", "eq": "active"},
            ]),
        );
        let assertions = parse_assertions(&template);
        assert_eq!(assertions.len(), 3);
        assert_eq!(assertions[0].field, "name");
        assert!(matches!(
            assertions[0].predicate,
            AssertionPredicate::NotNull
        ));
        assert_eq!(assertions[1].field, "balance");
        assert!(matches!(assertions[1].predicate, AssertionPredicate::Gte(v) if v == 0.0));
        assert_eq!(assertions[2].field, "status");
        assert!(matches!(&assertions[2].predicate, AssertionPredicate::Eq(s) if s == "active"));
    }

    #[test]
    fn parse_assertions_missing() {
        let template = nmbrs_workload::model::ParsedOp::simple("test", "INSERT");
        assert!(parse_assertions(&template).is_empty());
    }
}
