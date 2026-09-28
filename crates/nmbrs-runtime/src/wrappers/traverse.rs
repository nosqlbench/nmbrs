// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Default result-traversal wrapper. Always wraps the inner
//! adapter dispenser: counts result elements + bytes, walks
//! declared capture points, and writes extracted values onto
//! the per-fiber op-template kernel via `ctx.wires.write`.

use std::collections::HashMap;
use std::sync::Arc;

use crate::adapter::WrappingDispenser;
use crate::adapter::{ExecutionError, OpDispenser, OpResult};
use crate::wrapper_registry::{WrapperName, WrapperRegistration, WrapperSubject};
use nmbrs_workload::bindpoints;

/// SRD-32a wrapper name. Innermost layer; always present.
pub const NAME: WrapperName = WrapperName::new("traverse");

/// Trigger: always — every op gets a traversal layer so result
/// bodies are consumed (and per-row metrics record element /
/// byte counts) even if the workload didn't ask for anything.
fn triggers(s: WrapperSubject) -> bool {
    s.op().is_some()
}

/// No per-op assignment text — the wrapper has no operator-
/// configurable knobs.
fn describe_assignment(_: WrapperSubject) -> Option<String> {
    None
}

inventory::submit! {
    WrapperRegistration {
        name: NAME,
        owned_fields: &[],
        triggers,
        requires_inner: &[],
        forbids_outer: &[],
        mutually_exclusive_with: &[],
        describe_assignment,
        levels: &[crate::wrapper_registry::WrapperLevel::Op],
    }
}

/// Result traversal statistics, backed by activity metrics counters.
pub struct TraversalStats {
    pub metrics: Arc<crate::activity::ActivityMetrics>,
}

/// Wraps an inner OpDispenser with result traversal and optional
/// capture extraction.
///
/// This is the default wrapper, always applied unless disabled.
/// It ensures that:
/// 1. The result body is fully consumed (element/byte counting)
/// 2. Captures are extracted from the result (if declared)
/// 3. Traversal metrics are recorded
pub struct TraversingDispenser {
    inner: Arc<dyn OpDispenser>,
    stats: Arc<TraversalStats>,
    /// Capture points parsed from the template at init time.
    /// Empty if no captures are declared.
    captures: Vec<bindpoints::CapturePoint>,
    /// The op's `traverse:` block, if it declared one. Absent means defaults —
    /// the layer is always installed, so this only tunes it.
    spec: Option<nmbrs_workload::model::TraverseSpec>,
}

impl TraversingDispenser {
    /// Wrap an inner dispenser with traversal.
    ///
    /// Reads `template.captures` (the parse-time-extracted capture
    /// specs) directly. The op-template parser has already stripped
    /// `[name]` / `[@name]` brackets from the op text fields, so
    /// adapters see clean SQL/URL/body strings.
    pub fn wrap(
        inner: Arc<dyn OpDispenser>,
        template: &nmbrs_workload::model::ParsedOp,
        stats: Arc<TraversalStats>,
    ) -> Arc<dyn OpDispenser> {
        Arc::new(Self {
            inner,
            stats,
            captures: template.captures.clone(),
            spec: template.traverse.clone(),
        })
    }
}

/// Extract captures from a result body's JSON.
///
/// Walks each declared capture spec against the body. Two modes:
///
/// - **Single** (`[name]`): take the first matching value. For an
///   array-of-rows body shape (CQL's standard JSON form), reads
///   row[0].name. For an object body, reads top-level `name`. For
///   wildcard `*`, captures every top-level field.
/// - **Slurp** (`[@name]`): walks every row of an array-of-rows
///   body and collects each row's column into a single
///   `Value::Json(array)`. Object bodies produce a single-element
///   list. This is the convenient shape for downstream consumers
///   that need all per-row values as a list (e.g. recall
///   evaluator's `actual:` reads).
///
/// The body's `.to_json()` form is the source of truth — adapters
/// that produce typed-row data render to a JSON array of row
/// objects.
#[cfg(test)]
fn extract_captures_from_json(
    body: &dyn crate::adapter::ResultBody,
    specs: &[bindpoints::CapturePoint],
) -> HashMap<String, polydat::ast::Value> {
    extract_captures_rooted(body, specs, None)
}

/// As [`extract_captures_from_json`], with an optional `traverse.path:` base
/// pointer applied to the body FIRST.
///
/// Re-roots the document rather than prefixing each capture: every capture
/// then addresses relative to the base, which is the whole point of the knob
/// (`{value, status, …}` envelopes force the same prefix onto every capture).
/// A base that does not resolve yields no captures at all — the caller's
/// `on_missing` policy decides whether that is worth saying out loud.
fn extract_captures_rooted(
    body: &dyn crate::adapter::ResultBody,
    specs: &[bindpoints::CapturePoint],
    base: Option<&str>,
) -> HashMap<String, polydat::ast::Value> {
    if specs.is_empty() {
        return HashMap::new();
    }
    let full = body.to_json();
    let json = match base.filter(|b| !b.is_empty()) {
        None => full,
        Some(b) => match full.pointer(b) {
            Some(sub) => sub.clone(),
            None => return HashMap::new(),
        },
    };
    let mut captures = HashMap::new();
    for spec in specs {
        // Declarative `capture:` block form: JSON-Pointer path
        // takes precedence over the bracket-source-name path.
        // Lets a workload address Jolokia bulk-POST responses
        // (`[{value:N}, {value:[...]}, {value:K}]`) by index +
        // nested field without re-shaping the response.
        if let Some(path) = spec.path.as_deref() {
            let sub = json.pointer(path);
            let value = if spec.count {
                polydat::ast::Value::U64(count_of_subtree(sub))
            } else if let Some(agg) = &spec.agg {
                aggregate_rows(sub, agg, spec.row_filter.as_ref())
            } else {
                match sub {
                    Some(v) => json_subtree_to_value(v),
                    None => polydat::ast::Value::None,
                }
            };
            captures.insert(spec.as_name.clone(), value);
            continue;
        }
        if spec.slurp {
            // Slurp form: collect across all rows.
            let collected = slurp_column(&json, &spec.source_name);
            captures.insert(
                spec.as_name.clone(),
                polydat::ast::Value::Json(std::sync::Arc::new(serde_json::Value::Array(collected))),
            );
            continue;
        }
        // Single form.
        if spec.source_name == "*" {
            // Wildcard: capture every top-level field. Falls
            // through to scalar-form per field.
            let target = match &json {
                serde_json::Value::Array(rows) => {
                    rows.first().cloned().unwrap_or(serde_json::Value::Null)
                }
                other => other.clone(),
            };
            if let serde_json::Value::Object(map) = target {
                for (k, v) in map {
                    captures.insert(k, json_to_value(&v));
                }
            }
            continue;
        }
        if let Some(val) = first_row_field(&json, &spec.source_name) {
            captures.insert(spec.as_name.clone(), json_to_value(&val));
        }
    }
    captures
}

/// Reduce an addressed JSON sub-tree to a u64 count, mirroring
/// the polling wrapper's `count_from_json_pointer` semantics:
/// array → length, object → key count, scalar → 1 (non-empty)
/// or 0 (false / empty string / zero number), null / missing →
/// 0. Use cases: `capture: { active_count: "/value:count" }`
/// reduces a list-of-running-jobs to a numeric gauge in one
/// step.
fn count_of_subtree(v: Option<&serde_json::Value>) -> u64 {
    let Some(v) = v else { return 0 };
    match v {
        serde_json::Value::Array(a) => a.len() as u64,
        serde_json::Value::Object(m) => m.len() as u64,
        serde_json::Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_i64().map(|i| i.max(0) as u64))
            .or_else(|| n.as_f64().map(|f| f.max(0.0) as u64))
            .unwrap_or(0),
        serde_json::Value::Bool(b) => {
            if *b {
                1
            } else {
                0
            }
        }
        serde_json::Value::String(s) if s.is_empty() => 0,
        serde_json::Value::String(_) => 1,
        serde_json::Value::Null => 0,
    }
}

/// Project a JSON sub-tree into a Polydat [`Value`]. Scalars go
/// through [`json_to_value`]'s typed coercion; structural
/// shapes (array / object) are kept as `Value::Json` so the
/// kernel can carry the original shape without lossy
/// stringification.
/// Folds one numeric field across the rows of an addressed array
/// (`:min(f)` / `:max(f)` / `:sum(f)` capture aggregation). Rows
/// missing the field, and non-numeric values, are skipped; an
/// empty fold yields `Value::None` (renders like a missing
/// capture). Sum stays integral (`U64`) while every contributing
/// value is a non-negative integer, else widens to `F64`; min/max
/// return the winning row's value with its original JSON type.
/// The capture values an op should publish when it SUCCEEDED but returned no
/// body at all — an empty CQL result set, an HTTP 204, an accepted timeout.
///
/// Skipping the write (the original behaviour) leaves every capture holding
/// its PREVIOUS value, so a wire that is being watched for "went to zero" can
/// never read zero once it has been nonzero. That pinned a compaction drain's
/// task count above zero permanently and hung the poll: cqlsh showed an empty
/// table while the workload's wire still read the last nonzero count.
///
/// An empty measurement is a measurement, and every fold is a commutative
/// monoid — the empty fold publishes the monoid's identity, TOTALLY:
///
/// - `count` / `sum` — the identity is mathematically forced: **0**.
/// - `min` / `max` — over an unbounded domain the identity is an extreme
///   (⊤ / ⊥) that would poison displays and metric series; the AUTHOR's
///   identity is the wire's own declared initial value. These return
///   `Value::None` here as a *reset marker*, and [`publish_capture`]
///   translates it into `wires.reset(name)` — the wire returns to its
///   declared unit instead of holding a raw `None` that every downstream
///   consumer would have to be None-aware of.
///
/// Folding any later real observation into a reset wire behaves as folding
/// into the unit — the monoid law the drain polls and the SRD-93 pressure
/// servo both lean on.
fn captures_for_empty_result(
    specs: &[bindpoints::CapturePoint],
) -> std::collections::HashMap<String, polydat::ast::Value> {
    use bindpoints::CaptureAgg;
    let mut out = std::collections::HashMap::new();
    for spec in specs {
        let value = if spec.count {
            polydat::ast::Value::U64(0)
        } else {
            match &spec.agg {
                Some(CaptureAgg::Sum(_)) => polydat::ast::Value::U64(0),
                Some(CaptureAgg::Min(_)) | Some(CaptureAgg::Max(_)) => polydat::ast::Value::None,
                None => polydat::ast::Value::None,
            }
        };
        out.insert(spec.as_name.clone(), value);
    }
    out
}

/// The single capture-publish chokepoint: a real value writes; the
/// `Value::None` reset marker (an empty min/max fold, or a plain capture
/// that resolved to nothing) RESTORES the wire to its declared initial
/// value. No path parks a raw `None` on a typed wire.
fn publish_capture(ctx: &crate::adapter::ExecCtx<'_>, name: &str, value: polydat::ast::Value) {
    if matches!(value, polydat::ast::Value::None) {
        let _ = ctx.wires.reset(name);
    } else {
        let _ = ctx.wires.write(name, value);
    }
}

fn aggregate_rows(
    sub: Option<&serde_json::Value>,
    agg: &bindpoints::CaptureAgg,
    row_filter: Option<&(String, String)>,
) -> polydat::ast::Value {
    use bindpoints::CaptureAgg;
    let rows = match sub {
        Some(serde_json::Value::Array(rows)) => rows,
        _ => return polydat::ast::Value::None,
    };
    let field = match agg {
        CaptureAgg::Min(f) | CaptureAgg::Max(f) | CaptureAgg::Sum(f) => f.as_str(),
    };
    // `where <field>='<value>'` — drop rows that are not commensurable before
    // folding. A result set can mix them: `system_views.sstable_tasks` lists a
    // data compaction in `unit=bytes` beside an index build in
    // `unit=token range parts`, and summing across both adds unlike
    // quantities. Compared as strings so it works on any scalar column
    // without the capture layer needing the column's type.
    let keep = |row: &serde_json::Value| -> bool {
        let Some((k, want)) = row_filter else {
            return true;
        };
        match row.get(k.as_str()) {
            Some(serde_json::Value::String(s)) => s == want,
            Some(other) => other.to_string().trim_matches('"') == want,
            None => false,
        }
    };
    let nums: Vec<&serde_json::Number> = rows
        .iter()
        .filter(|row| keep(row))
        .filter_map(|row| row.get(field))
        .filter_map(|v| v.as_number())
        .collect();
    if nums.is_empty() {
        // Same totality contract as `captures_for_empty_result`, applied to
        // the rows-present-but-none-match case — the two empty paths used to
        // disagree (`:sum` gave 0 with no body but None past a `where` that
        // matched nothing), and a `None` parked on a typed wire forced every
        // downstream consumer to be None-aware. `sum` publishes its forced
        // identity 0; `min`/`max` return the `None` RESET MARKER, which
        // `publish_capture` turns into a reset to the wire's declared
        // initial value (the author's identity element).
        return match agg {
            CaptureAgg::Sum(_) => polydat::ast::Value::U64(0),
            CaptureAgg::Min(_) | CaptureAgg::Max(_) => polydat::ast::Value::None,
        };
    }
    match agg {
        CaptureAgg::Sum(_) => {
            if nums.iter().all(|n| n.as_u64().is_some()) {
                polydat::ast::Value::U64(nums.iter().map(|n| n.as_u64().unwrap()).sum())
            } else {
                polydat::ast::Value::F64(nums.iter().filter_map(|n| n.as_f64()).sum())
            }
        }
        CaptureAgg::Min(_) | CaptureAgg::Max(_) => {
            let want_min = matches!(agg, CaptureAgg::Min(_));
            let mut best = nums[0];
            for n in &nums[1..] {
                let (a, b) = (
                    n.as_f64().unwrap_or(f64::NAN),
                    best.as_f64().unwrap_or(f64::NAN),
                );
                if (want_min && a < b) || (!want_min && a > b) {
                    best = n;
                }
            }
            json_to_value(&serde_json::Value::Number((*best).clone()))
        }
    }
}

fn json_subtree_to_value(v: &serde_json::Value) -> polydat::ast::Value {
    match v {
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            polydat::ast::Value::Json(std::sync::Arc::new(v.clone()))
        }
        scalar => json_to_value(scalar),
    }
}

/// First-row lookup: for an array body, read `rows[0].name`; for
/// an object body, read `obj.name`. Returns `None` when the field
/// isn't present.
fn first_row_field(json: &serde_json::Value, name: &str) -> Option<serde_json::Value> {
    match json {
        serde_json::Value::Array(rows) => rows.first().and_then(|row| row.get(name)).cloned(),
        serde_json::Value::Object(_) => json.get(name).cloned(),
        _ => None,
    }
}

/// Slurp helper: walk an array body and collect each row's `name`
/// field. Object bodies produce a single-element list. Non-object,
/// non-array bodies produce an empty list.
fn slurp_column(json: &serde_json::Value, name: &str) -> Vec<serde_json::Value> {
    match json {
        serde_json::Value::Array(rows) => rows
            .iter()
            .filter_map(|row| row.get(name).cloned())
            .collect(),
        serde_json::Value::Object(_) => json.get(name).map(|v| vec![v.clone()]).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Convert a serde_json::Value to a Polydat Value. JSON `null`
/// maps to [`Value::None`] (per SRD-74 — None propagates
/// rather than coercing to the string `"null"`); arrays and
/// objects stringify only when reached via the row-shape
/// extraction path (the JSON-Pointer extraction route uses
/// [`json_subtree_to_value`] which preserves them as
/// `Value::Json`).
pub(crate) fn json_to_value(v: &serde_json::Value) -> polydat::ast::Value {
    match v {
        serde_json::Value::Null => polydat::ast::Value::None,
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_u64() {
                polydat::ast::Value::U64(i)
            } else if let Some(f) = n.as_f64() {
                polydat::ast::Value::F64(f)
            } else {
                polydat::ast::Value::Str(n.to_string().into())
            }
        }
        serde_json::Value::Bool(b) => polydat::ast::Value::Bool(*b),
        serde_json::Value::String(s) => polydat::ast::Value::Str(s.as_str().into()),
        other => polydat::ast::Value::Str(other.to_string().into()),
    }
}

impl WrappingDispenser for TraversingDispenser {}

impl OpDispenser for TraversingDispenser {
    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a crate::fixture::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            // Execute the inner dispenser
            let result = self.inner.execute(cycle, ctx).await?;

            // Traverse: count elements and bytes
            if let Some(body) = &result.body {
                self.stats
                    .metrics
                    .result_elements
                    .inc_by(body.element_count());
                if let Some(bytes) = body.byte_count() {
                    self.stats.metrics.result_bytes.inc_by(bytes);
                }
            }

            // Extract captures from result if declared. Values land
            // on the per-fiber kernel's input slot via ctx.wires.write;
            // wrappers above this layer (e.g. MetricsDispenser) see
            // them through wires.get on the same cycle.
            // A SUCCEEDING op that returned no body still measured something:
            // zero rows. Publish the identity values rather than skipping the
            // write, or every capture silently keeps its previous reading and a
            // "wait for it to reach zero" poll can never succeed.
            if !self.captures.is_empty() && result.body.is_none() {
                for (name, value) in captures_for_empty_result(&self.captures) {
                    publish_capture(&ctx, &name, value);
                }
            }
            if !self.captures.is_empty()
                && let Some(body) = &result.body
            {
                let base = self.spec.as_ref().and_then(|s| s.path.as_deref());
                let extracted = extract_captures_rooted(body.as_ref(), &self.captures, base);
                // `on_missing:` — a capture that resolved to nothing reads
                // exactly like one that measured an absence, which is how a
                // renamed column or a changed schema hides. Default stays
                // `ignore` so existing workloads are unaffected.
                let policy = self.spec.as_ref().map(|s| s.on_missing).unwrap_or_default();
                if !matches!(policy, nmbrs_workload::model::OnMissing::Ignore) {
                    for cp in &self.captures {
                        let missing = match extracted.get(&cp.as_name) {
                            None => true,
                            Some(polydat::ast::Value::None) => true,
                            Some(_) => false,
                        };
                        if !missing {
                            continue;
                        }
                        let detail = format!(
                            "op capture '{}' resolved to nothing{}",
                            cp.as_name,
                            match base {
                                Some(b) => format!(" (traverse path '{b}')"),
                                None => String::new(),
                            }
                        );
                        match policy {
                            nmbrs_workload::model::OnMissing::Warn => {
                                crate::diag!(crate::observer::LogLevel::Warn, "{detail}")
                            }
                            nmbrs_workload::model::OnMissing::Error => {
                                return Err(ExecutionError::Op(crate::adapter::AdapterError {
                                    error_name: "capture_missing".into(),
                                    message: detail,
                                    retryable: false,
                                }));
                            }
                            nmbrs_workload::model::OnMissing::Ignore => {}
                        }
                    }
                }
                for (name, value) in extracted {
                    publish_capture(&ctx, &name, value);
                }
            }

            Ok(result)
        })
    }
    fn inner_dispenser(&self) -> Option<&dyn OpDispenser> {
        Some(self.inner.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::ResultBody;

    fn agg_cap(
        alias: &str,
        agg: Option<bindpoints::CaptureAgg>,
        count: bool,
    ) -> bindpoints::CapturePoint {
        bindpoints::CapturePoint {
            row_filter: None,
            source_name: alias.into(),
            as_name: alias.into(),
            cast_type: None,
            slurp: false,
            path: Some(String::new()),
            count,
            agg,
        }
    }

    /// An op that SUCCEEDS with no body measured zero rows, and must publish
    /// that. Skipping the write leaves every capture holding its previous
    /// reading, so a wire watched for "went to zero" can never reach zero once
    /// it has been nonzero — which pinned a compaction drain's task count above
    /// zero permanently and hung its poll for 275s+ against a 48h timeout,
    /// while cqlsh showed the table empty.
    #[test]
    fn empty_result_publishes_identity_for_count_and_sum() {
        let specs = vec![
            agg_cap("n", None, true),
            agg_cap("s", Some(bindpoints::CaptureAgg::Sum("x".into())), false),
        ];
        let out = super::captures_for_empty_result(&specs);
        assert!(
            matches!(out.get("n"), Some(polydat::ast::Value::U64(0))),
            "counting nothing is 0, not 'leave the old count': {:?}",
            out.get("n")
        );
        assert!(
            matches!(out.get("s"), Some(polydat::ast::Value::U64(0))),
            "summing nothing is 0: {:?}",
            out.get("s")
        );
    }

    /// min/max of nothing has NO honest value, so they publish None — which
    /// clears the wire rather than leaving a stale reading that is
    /// indistinguishable from a real observation. Writing 0 would be a lie:
    /// zero is a plausible minimum.
    #[test]
    fn empty_result_clears_min_and_max_rather_than_inventing_a_value() {
        let specs = vec![
            agg_cap("lo", Some(bindpoints::CaptureAgg::Min("x".into())), false),
            agg_cap("hi", Some(bindpoints::CaptureAgg::Max("x".into())), false),
        ];
        let out = super::captures_for_empty_result(&specs);
        assert!(
            matches!(out.get("lo"), Some(polydat::ast::Value::None)),
            "min of nothing must not invent 0: {:?}",
            out.get("lo")
        );
        assert!(
            matches!(out.get("hi"), Some(polydat::ast::Value::None)),
            "max of nothing must not invent 0: {:?}",
            out.get("hi")
        );
    }

    /// A plain (non-aggregate) capture over an empty result is simply absent.
    #[test]
    fn empty_result_leaves_plain_captures_absent() {
        let specs = vec![agg_cap("v", None, false)];
        let out = super::captures_for_empty_result(&specs);
        assert!(matches!(out.get("v"), Some(polydat::ast::Value::None)));
    }

    /// Rows that EXIST but match nothing stay `None`, deliberately — a
    /// different statement from "there was no result at all", and the
    /// pre-existing rule (`aggregate_filter_matching_no_rows_yields_none`).
    /// That case was never the staleness bug: this value IS written to the
    /// wire, so it clears. Only the no-body path skipped the write.
    #[test]
    fn empty_fold_totality_sum_is_zero_min_max_are_reset_markers() {
        // The monoid contract (SRD-93-era fold totality): sum-of-nothing
        // publishes its forced identity 0; min/max-of-nothing return the
        // `None` RESET MARKER that `publish_capture` translates into a
        // reset to the wire's declared initial value — no path parks a
        // raw None on a typed wire, and no path leaves a stale reading.
        let rows = serde_json::json!([{"other": 1}, {"other": 2}]);
        let v = super::aggregate_rows(
            Some(&rows),
            &bindpoints::CaptureAgg::Sum("missing".into()),
            None,
        );
        assert!(
            matches!(v, polydat::ast::Value::U64(0)),
            "sum-of-nothing is its identity 0: {v:?}"
        );
        for agg in [
            bindpoints::CaptureAgg::Min("missing".into()),
            bindpoints::CaptureAgg::Max("missing".into()),
        ] {
            let v = super::aggregate_rows(Some(&rows), &agg, None);
            assert!(
                matches!(v, polydat::ast::Value::None),
                "min/max-of-nothing is the reset marker: {v:?}"
            );
        }
    }

    fn cap(source: &str, alias: &str, slurp: bool) -> bindpoints::CapturePoint {
        bindpoints::CapturePoint {
            row_filter: None,
            source_name: source.into(),
            as_name: alias.into(),
            cast_type: None,
            slurp,
            path: None,
            count: false,
            agg: None,
        }
    }

    #[test]
    fn parse_captures_from_template() {
        let parsed =
            bindpoints::parse_capture_points("SELECT [username], [age as user_age] FROM users");
        assert_eq!(parsed.captures.len(), 2);
        assert_eq!(parsed.captures[0].source_name, "username");
        assert_eq!(parsed.captures[0].as_name, "username");
        assert!(!parsed.captures[0].slurp);
        assert_eq!(parsed.captures[1].source_name, "age");
        assert_eq!(parsed.captures[1].as_name, "user_age");
        assert_eq!(parsed.raw_template, "SELECT username, age FROM users");
    }

    #[test]
    fn parse_slurp_capture() {
        let parsed = bindpoints::parse_capture_points("SELECT [@keys] FROM t");
        assert_eq!(parsed.captures.len(), 1);
        assert_eq!(parsed.captures[0].source_name, "keys");
        assert!(parsed.captures[0].slurp);
        assert_eq!(parsed.raw_template, "SELECT keys FROM t");
    }

    #[derive(Debug)]
    struct JsonBody(serde_json::Value);
    impl ResultBody for JsonBody {
        fn to_json(&self) -> serde_json::Value {
            self.0.clone()
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[test]
    fn extract_from_json_top_level() {
        let body = JsonBody(serde_json::json!({
            "user_id": 42,
            "name": "alice",
            "balance": 99.5
        }));
        let specs = vec![cap("user_id", "uid", false), cap("name", "name", false)];
        let captures = extract_captures_from_json(&body, &specs);
        assert_eq!(captures.len(), 2);
        assert_eq!(captures["uid"].as_u64(), 42);
        match &captures["name"] {
            polydat::ast::Value::Str(s) => assert_eq!(&**s, "alice"),
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn extract_wildcard() {
        let body = JsonBody(serde_json::json!({"a": 1, "b": 2}));
        let specs = vec![cap("*", "*", false)];
        let captures = extract_captures_from_json(&body, &specs);
        assert_eq!(captures.len(), 2);
    }

    #[test]
    fn extract_slurp_array_of_rows() {
        let body = JsonBody(serde_json::json!([
            {"key": 4, "value": 0.5},
            {"key": 17, "value": 0.4},
            {"key": 42, "value": 0.3},
        ]));
        let specs = vec![cap("key", "key", true)];
        let captures = extract_captures_from_json(&body, &specs);
        assert_eq!(captures.len(), 1);
        match &captures["key"] {
            polydat::ast::Value::Json(arc) => {
                let serde_json::Value::Array(items) = arc.as_ref() else {
                    panic!("expected Value::Json(array), got {arc:?}");
                };
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], serde_json::json!(4));
                assert_eq!(items[1], serde_json::json!(17));
                assert_eq!(items[2], serde_json::json!(42));
            }
            other => panic!("expected Value::Json(array), got {other:?}"),
        }
    }

    #[test]
    fn extract_single_first_row_of_array() {
        let body = JsonBody(serde_json::json!([
            {"key": 4}, {"key": 17}, {"key": 42},
        ]));
        let specs = vec![cap("key", "first_key", false)];
        let captures = extract_captures_from_json(&body, &specs);
        assert_eq!(captures.len(), 1);
        assert_eq!(captures["first_key"].as_u64(), 4);
    }

    fn cap_path(name: &str, path: &str, count: bool) -> bindpoints::CapturePoint {
        bindpoints::CapturePoint {
            source_name: name.into(),
            as_name: name.into(),
            cast_type: None,
            slurp: false,
            path: Some(path.into()),
            count,
            agg: None,
            row_filter: None,
        }
    }

    /// `traverse.path:` re-roots the body, so a capture addresses relative to
    /// it instead of repeating the envelope prefix on every line.
    #[test]
    fn traverse_path_re_roots_the_document() {
        let body = JsonBody(serde_json::json!({
            "status": 200,
            "value": [{"progress": 7u64}],
        }));
        let spec = |name: &str, path: &str| bindpoints::CapturePoint {
            source_name: name.into(),
            as_name: name.into(),
            cast_type: None,
            slurp: false,
            path: Some(path.to_string()),
            count: false,
            agg: None,
            row_filter: None,
        };
        // Without the base, the capture must carry the whole prefix.
        let long = extract_captures_rooted(&body, &[spec("p", "/value/0/progress")], None);
        assert_eq!(long["p"].as_u64(), 7);

        // With it, the same value is addressed relative to `/value`.
        let short = extract_captures_rooted(&body, &[spec("p", "/0/progress")], Some("/value"));
        assert_eq!(short["p"].as_u64(), 7);
    }

    /// A base path that does not resolve yields no captures — the caller's
    /// `on_missing` decides whether that is worth saying out loud.
    #[test]
    fn an_unresolvable_traverse_path_yields_nothing() {
        let body = JsonBody(serde_json::json!({"value": 1}));
        let spec = bindpoints::CapturePoint {
            source_name: "x".into(),
            as_name: "x".into(),
            cast_type: None,
            slurp: false,
            path: Some("/x".into()),
            count: false,
            agg: None,
            row_filter: None,
        };
        let caps = extract_captures_rooted(&body, &[spec], Some("/nonesuch"));
        assert!(caps.is_empty(), "unresolvable base must not invent values");
    }

    /// The same shape, folded PER KIND. Summing across the two rows adds
    /// bytes to token range parts — dimensionally meaningless, and its
    /// magnitude depends on how many tasks were registered at sample time.
    /// `where kind='…'` keeps each fold to one unit.
    #[test]
    fn aggregate_captures_filter_rows_by_kind() {
        let body = JsonBody(serde_json::json!([
            {"kind": "compaction",            "progress": 47048035855u64},
            {"kind": "secondary index build", "progress": 4855601u64},
        ]));
        fn cap_filtered(
            name: &str,
            field: &str,
            filter: Option<(&str, &str)>,
        ) -> bindpoints::CapturePoint {
            bindpoints::CapturePoint {
                source_name: name.into(),
                as_name: name.into(),
                cast_type: None,
                slurp: false,
                path: Some(String::new()),
                count: false,
                agg: Some(bindpoints::CaptureAgg::Sum(field.into())),
                row_filter: filter.map(|(k, v)| (k.to_string(), v.to_string())),
            }
        }
        let specs = vec![
            cap_filtered("all", "progress", None),
            cap_filtered("index", "progress", Some(("kind", "secondary index build"))),
            cap_filtered("data", "progress", Some(("kind", "compaction"))),
        ];
        let caps = extract_captures_from_json(&body, &specs);
        let num = |k: &str| caps[k].as_u64();
        assert_eq!(num("index"), 4_855_601, "index build only");
        assert_eq!(num("data"), 47_048_035_855, "data compaction only");
        // The unfiltered fold is the sum of unlike quantities — kept as the
        // record of what the old capture actually produced.
        assert_eq!(num("all"), num("index") + num("data"));
    }

    /// A predicate that matches nothing folds to absent, not to zero — zero
    /// would read as "measured, and it was none".
    #[test]
    fn aggregate_filter_matching_no_rows_folds_to_sum_identity() {
        let body = JsonBody(serde_json::json!([
            {"kind": "compaction", "progress": 5u64},
        ]));
        let specs = vec![bindpoints::CapturePoint {
            source_name: "x".into(),
            as_name: "x".into(),
            cast_type: None,
            slurp: false,
            path: Some(String::new()),
            count: false,
            agg: Some(bindpoints::CaptureAgg::Sum("progress".into())),
            row_filter: Some(("kind".into(), "nonesuch".into())),
        }];
        let caps = extract_captures_from_json(&body, &specs);
        // Fold totality: a `where` that matches nothing sums to the
        // identity 0 — the same statement the no-body path makes, where
        // the two paths used to disagree (0 vs a stale-prone None).
        assert!(
            matches!(caps["x"], polydat::ast::Value::U64(0)),
            "a filtered-empty sum folds to its identity 0; got {:?}",
            caps["x"]
        );
    }

    #[test]
    fn aggregate_captures_fold_rows_with_mixed_units() {
        // The sstable_tasks shape that motivated aggregation: a
        // byte-denominated task saturated at 1.0 alongside an
        // ordinal-denominated task mid-flight. min(ratio) must
        // surface the merge's 0.4, not row 0's 1.0.
        let body = JsonBody(serde_json::json!([
            {"completion_ratio": 1.0, "progress": 31357891323u64, "total": 31357891323u64},
            {"completion_ratio": 0.4, "progress": 8000000u64,     "total": 20000000u64},
        ]));
        fn cap_agg(name: &str, agg: bindpoints::CaptureAgg) -> bindpoints::CapturePoint {
            bindpoints::CapturePoint {
                source_name: name.into(),
                as_name: name.into(),
                cast_type: None,
                slurp: false,
                path: Some(String::new()),
                count: false,
                agg: Some(agg),
                row_filter: None,
            }
        }
        let specs = vec![
            cap_agg(
                "completion_ratio",
                bindpoints::CaptureAgg::Min("completion_ratio".into()),
            ),
            cap_agg(
                "max_ratio",
                bindpoints::CaptureAgg::Max("completion_ratio".into()),
            ),
            cap_agg("progress", bindpoints::CaptureAgg::Sum("progress".into())),
        ];
        let captures = extract_captures_from_json(&body, &specs);
        assert_eq!(captures["completion_ratio"].as_f64(), 0.4);
        assert_eq!(captures["max_ratio"].as_f64(), 1.0);
        assert_eq!(captures["progress"].as_u64(), 31357891323 + 8000000);
        // Empty result set folds to None (missing-capture semantics).
        let empty = JsonBody(serde_json::json!([]));
        let c2 = extract_captures_from_json(&empty, &specs[..1]);
        assert!(matches!(c2["completion_ratio"], polydat::ast::Value::None));
    }

    #[test]
    fn extract_json_pointer_scalar_from_bulk_response() {
        let body = JsonBody(serde_json::json!([
            {"value": 7,       "status": 200},
            {"value": [],      "status": 200},
            {"value": 0,       "status": 200},
        ]));
        let specs = vec![
            cap_path("sstables", "/0/value", false),
            cap_path("pending_for_cf", "/2/value", false),
        ];
        let captures = extract_captures_from_json(&body, &specs);
        assert_eq!(captures["sstables"].as_u64(), 7);
        assert_eq!(captures["pending_for_cf"].as_u64(), 0);
    }

    #[test]
    fn extract_json_pointer_resolved_null_yields_none_not_string() {
        let body = JsonBody(serde_json::json!([
            {"value": null, "status": 200},
        ]));
        let specs = vec![cap_path("pending_for_cf", "/0/value", false)];
        let captures = extract_captures_from_json(&body, &specs);
        assert!(
            matches!(captures["pending_for_cf"], polydat::ast::Value::None),
            "JSON null at path should yield Value::None, got {:?}",
            captures["pending_for_cf"],
        );
    }

    #[test]
    fn extract_json_pointer_count_on_resolved_null_returns_zero() {
        let body = JsonBody(serde_json::json!([
            {"value": null, "status": 200},
        ]));
        let specs = vec![cap_path("pending_for_cf", "/0/value", true)];
        let captures = extract_captures_from_json(&body, &specs);
        assert_eq!(
            captures["pending_for_cf"].as_u64(),
            0,
            "`:count` on resolved-null should return 0, got {:?}",
            captures["pending_for_cf"],
        );
    }

    #[test]
    fn extract_json_pointer_count_collapses_array_to_length() {
        let body = JsonBody(serde_json::json!([
            {"value": 7},
            {"value": [
                {"compactionId":"a", "keyspace":"ks", "columnfamily":"cf"},
                {"compactionId":"b", "keyspace":"ks", "columnfamily":"cf"},
            ]},
        ]));
        let specs = vec![cap_path("active_count", "/1/value", true)];
        let captures = extract_captures_from_json(&body, &specs);
        assert_eq!(captures["active_count"].as_u64(), 2);
    }

    #[test]
    fn extract_json_pointer_missing_path_yields_none() {
        let body = JsonBody(serde_json::json!({"value": 7}));
        let specs = vec![cap_path("not_there", "/missing/path", false)];
        let captures = extract_captures_from_json(&body, &specs);
        assert!(
            matches!(captures["not_there"], polydat::ast::Value::None),
            "expected Value::None for unresolvable JSON-Pointer, got {:?}",
            captures["not_there"],
        );
    }

    #[test]
    fn extract_json_pointer_count_of_missing_path_is_zero() {
        let body = JsonBody(serde_json::json!({"value": 7}));
        let specs = vec![cap_path("active_count", "/missing/value", true)];
        let captures = extract_captures_from_json(&body, &specs);
        assert_eq!(captures["active_count"].as_u64(), 0);
    }

    #[test]
    fn extract_json_pointer_structural_sub_tree_captured_as_json() {
        let body = JsonBody(serde_json::json!([
            {"value": {"keyspace": "ks", "table": "cf", "ssTables": 3}},
        ]));
        let specs = vec![cap_path("state", "/0/value", false)];
        let captures = extract_captures_from_json(&body, &specs);
        match &captures["state"] {
            polydat::ast::Value::Json(arc) => {
                assert_eq!(arc.get("keyspace").and_then(|v| v.as_str()), Some("ks"));
                assert_eq!(arc.get("ssTables").and_then(|v| v.as_u64()), Some(3));
            }
            other => panic!("expected Value::Json, got {other:?}"),
        }
        // Silence dead-code warnings if helpers are unused in some subset
        let _ = count_of_subtree;
        let _ = json_subtree_to_value;
    }
}
