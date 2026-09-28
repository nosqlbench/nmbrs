// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Load-time authoring lints for semantic surfaces serde cannot check
//! (SRD-83 follow-up). Runs once at workload load — before any phase
//! dispatch, dryrun included — per the dryrun-as-validation-floor
//! doctrine and "never ignore silently".
//!
//! Three checks:
//!
//! 1. **Error-router specs parse.** Every `errors:` spec — phase-level
//!    and op-level — goes through the real [`ErrorRouter`] parser, so a
//!    bad verb or regex is a load error naming its phase/op instead of
//!    a first-error-at-runtime surprise. Specs carrying `{param}`
//!    interpolation are skipped here (they resolve later and are
//!    parsed again at scope init).
//! 2. **Error-router catch-all lint.** An error class matching no rule
//!    falls through to `stop` with only an eprintln; a router without a
//!    literal `.*` rule therefore has a silent fall-through mode. One
//!    warning per such spec.
//! 3. **Metric-family lint.** `metric('family, …', 'stat')` and
//!    `metric_window(…)` selectors inside `stop_when`, `continue_if`,
//!    and phase `poll.until` predicates read 0.0 SILENTLY when the
//!    family never registers — a typo'd family makes a coordination
//!    gate pass instantly or hang to its timeout. Family tokens are
//!    checked against the built-in activity instrument namespace plus
//!    every phase-declared `metrics:` name. Unknown families WARN
//!    rather than fail: adapter counters and relevancy families are
//!    registered at runtime and are not statically knowable.
//!
//! Hard failures come back as `Err`; warnings come back as strings for
//! the caller to route through `diag!` (keeps this module pure and
//! directly testable).

use nmbrs_workload::model::{StopConditionSpec, WorkloadPhase};

/// Built-in activity instrument families, mirroring
/// `ActivityMetrics::register_on` (activity.rs) — the same namespace the
/// SRD-83 stop-condition wires draw from. Phase-declared `metrics:`
/// names are unioned in per workload before matching.
const ACTIVITY_FAMILIES: [&str; 18] = [
    "cycles_servicetime",
    "cycles_waittime",
    "cycles_responsetime",
    "result_success",
    "result_failure",
    "result_total",
    "cycles_total",
    "skips_total",
    "errors_total",
    "attempt_total",
    "attempt_success",
    "attempt_failure",
    "stanzas_total",
    "daemon_cancelled_total",
    "daemon_errors_total",
    "result_elements",
    "result_bytes",
    "tries",
];

/// Run every lint over the loaded workload. `Ok(warnings)` — the run
/// may proceed, with each warning routed to the operator; `Err` — a
/// spec is malformed and the load fails.
pub fn lint_workload<'a>(
    workload_stop_when: &[StopConditionSpec],
    phases: impl Iterator<Item = (&'a str, &'a WorkloadPhase)> + Clone,
) -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();

    // The metric-family allowlist: built-ins + every phase-declared
    // phase-metric name across the workload (a gate in one phase may
    // legitimately read a metric another phase declares).
    let mut known: Vec<String> = ACTIVITY_FAMILIES.iter().map(|s| s.to_string()).collect();
    for (_, phase) in phases.clone() {
        known.extend(phase.metrics.keys().cloned());
    }

    for sc in workload_stop_when {
        lint_metric_families(&sc.when, &known, "workload `stop_when`", &mut warnings);
    }

    for (name, phase) in phases {
        let ctx = format!("phase '{name}'");
        if let Some(spec) = phase.errors.as_deref() {
            lint_error_router(spec, &ctx, &mut warnings)?;
        }
        for op in &phase.ops {
            if let Some(serde_json::Value::String(spec)) = op.params.get("errors") {
                lint_error_router(spec, &format!("{ctx} op '{}'", op.name), &mut warnings)?;
            }
        }
        for sc in &phase.stop_when {
            lint_metric_families(
                &sc.when,
                &known,
                &format!("{ctx} `stop_when`"),
                &mut warnings,
            );
        }
        if let Some(gate) = &phase.continue_if {
            lint_metric_families(
                &gate.when,
                &known,
                &format!("{ctx} `continue_if`"),
                &mut warnings,
            );
        }
        if let Some(poll) = &phase.poll {
            lint_metric_families(
                &poll.until,
                &known,
                &format!("{ctx} `poll.until`"),
                &mut warnings,
            );
        }
    }

    Ok(warnings)
}

/// Check one `errors:` router spec: it must parse (hard error), and it
/// should carry a catch-all rule (warning). Interpolated specs are
/// deferred to runtime resolution.
fn lint_error_router(spec: &str, ctx: &str, warnings: &mut Vec<String>) -> Result<(), String> {
    if spec.contains('{') {
        return Ok(()); // `{param}`-bearing; resolved and parsed later
    }
    let router = nmbrs_errorhandler::ErrorRouter::parse(spec)
        .map_err(|e| format!("{ctx}: invalid `errors:` spec '{spec}': {e}"))?;
    if !router.has_catch_all() {
        warnings.push(format!(
            "{ctx}: `errors: \"{spec}\"` has no catch-all rule — an error \
             class matching no pattern falls through to `stop`; end the \
             spec with `.*:<verbs>` to make the default explicit"
        ));
    }
    Ok(())
}

/// Scan one polydat predicate source for `metric(…)` /
/// `metric_window(…)` calls with a literal selector, and warn on family
/// tokens outside the known namespace.
fn lint_metric_families(src: &str, known: &[String], ctx: &str, warnings: &mut Vec<String>) {
    for family in metric_families(src) {
        if family.contains('{') {
            continue; // interpolated — resolved later
        }
        if !known.iter().any(|k| k == &family) {
            warnings.push(format!(
                "{ctx}: `metric()` selector names family '{family}', which is \
                 not a built-in instrument family or a declared phase metric — \
                 an unregistered family reads 0.0 silently; check the spelling \
                 (predicate: {src})"
            ));
        }
    }
}

/// Extract the family token (the first comma-separated field of the
/// selector literal) from every `metric('…')` / `metric_window('…')`
/// call in `src`. Non-literal selectors (an expression, not a quoted
/// string) yield nothing — those are resolved at runtime.
fn metric_families(src: &str) -> Vec<String> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel) = src[i..].find("metric") {
        let start = i + rel;
        i = start + "metric".len();
        // Word boundary on the left: reject e.g. `my_metric(`.
        if start > 0 {
            let prev = bytes[start - 1] as char;
            if prev.is_alphanumeric() || prev == '_' {
                continue;
            }
        }
        // Optional `_window` suffix, then `(`.
        let mut j = i;
        if src[j..].starts_with("_window") {
            j += "_window".len();
        }
        let rest = src[j..].trim_start();
        let Some(after_paren) = rest.strip_prefix('(') else {
            continue;
        };
        let sel = after_paren.trim_start();
        let Some(quote) = sel.chars().next().filter(|c| *c == '\'' || *c == '"') else {
            continue; // non-literal selector
        };
        let body = &sel[1..];
        let Some(end) = body.find(quote) else {
            continue;
        };
        let selector = &body[..end];
        let family = selector.split(',').next().unwrap_or("").trim().to_string();
        if !family.is_empty() {
            out.push(family);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_families_from_both_call_forms() {
        let src = "metric('cycles_total, phase=ingest', 'count') >= 5 \
                   && metric_window(\"result_failure\", 'rate') < 0.1";
        assert_eq!(
            metric_families(src),
            vec!["cycles_total".to_string(), "result_failure".to_string()]
        );
    }

    #[test]
    fn ignores_non_literal_selectors_and_other_identifiers() {
        assert!(metric_families("my_metric('x','count') + metric(fam, 'count')").is_empty());
    }

    #[test]
    fn unknown_family_warns_known_family_does_not() {
        let known: Vec<String> = ACTIVITY_FAMILIES.iter().map(|s| s.to_string()).collect();
        let mut w = Vec::new();
        lint_metric_families(
            "metric('cycles_totl, phase=x', 'count') > 0",
            &known,
            "t",
            &mut w,
        );
        assert_eq!(w.len(), 1, "typo'd family must warn: {w:?}");
        w.clear();
        lint_metric_families(
            "metric('cycles_total, phase=x', 'count') > 0",
            &known,
            "t",
            &mut w,
        );
        assert!(w.is_empty(), "known family must not warn: {w:?}");
    }

    #[test]
    fn router_without_catch_all_warns_with_catch_all_does_not() {
        let mut w = Vec::new();
        lint_error_router("TimeoutError:retry,warn", "t", &mut w).unwrap();
        assert_eq!(w.len(), 1, "no catch-all must warn: {w:?}");
        w.clear();
        lint_error_router("TimeoutError:retry,warn;.*:counter", "t", &mut w).unwrap();
        assert!(w.is_empty(), "catch-all present must not warn: {w:?}");
    }

    #[test]
    fn bad_router_spec_is_a_load_error_interpolated_is_deferred() {
        let mut w = Vec::new();
        let err = lint_error_router(".*:sotp", "phase 'p'", &mut w).unwrap_err();
        assert!(err.contains("unknown error handler"), "got: {err}");
        lint_error_router("{overload_policy}", "t", &mut w)
            .expect("interpolated spec must be deferred, not parsed");
    }
}
