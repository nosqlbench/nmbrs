// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-77 — query-construction rewrites.
//!
//! Every read-side surface that runs a metricsql expression
//! against the storage layer first hands the parsed AST
//! through here. The current pass: ensure every `MetricExpr`
//! (vector selector) carries an `exec_id` label filter. If the
//! operator didn't write one, inject the resolved
//! "latest"-execution id; if they wrote `exec_id="latest"`
//! explicitly, resolve it to the same concrete value.
//!
//! Why this rewrite is the right shape:
//! - **No silent storage pollution.** Storage never sees the
//!   `"latest"` literal — the metric_instance reserved-word
//!   guard refuses to land it. The injection happens at the
//!   construction boundary, where the resolver knows the
//!   target session's max(exec_id).
//! - **Operator-typed queries stay verbatim.** If the user
//!   wrote `exec_id="3"`, we leave it alone. The rewrite only
//!   touches missing/`"latest"` entries.
//! - **Aggregate is explicit.** Callers that want the
//!   aggregate-across-executions intent pass `None` (no
//!   injection) — the rewrite is a no-op and the operator's
//!   query runs verbatim.

use crate::ast::{Expr, LabelFilter, LabelFilterOp, MetricExpr};

/// Walk `expr` and ensure every `MetricExpr` (vector
/// selector) carries an `exec_id="<resolved_id>"` filter.
/// `resolved_id` is the concrete integer id to substitute
/// for missing or `"latest"`-valued matchers. `None` means
/// "aggregate across executions" — the rewrite is a no-op.
///
/// Idempotent: calling the rewrite twice with the same
/// `resolved_id` produces the same AST as one call.
pub fn inject_default_exec_id(expr: &mut Expr, resolved_id: Option<u64>) {
    let Some(id) = resolved_id else {
        return;
    };
    walk(expr, id);
}

fn walk(expr: &mut Expr, id: u64) {
    match expr {
        Expr::Metric(m) => inject_into_metric(m, id),
        Expr::Func(f) => {
            for arg in f.args.iter_mut() {
                walk(arg, id);
            }
        }
        Expr::Binary(b) => {
            walk(&mut b.left, id);
            walk(&mut b.right, id);
        }
        Expr::Paren(p) => {
            for e in p.exprs.iter_mut() {
                walk(e, id);
            }
        }
        Expr::Rollup(r) => walk(&mut r.expr, id),
        Expr::With(w) => {
            for binding in w.bindings.iter_mut() {
                walk(&mut binding.expr, id);
            }
            walk(&mut w.body, id);
        }
        // Leaf expressions carry no selectors.
        Expr::Number(_) | Expr::String(_) | Expr::Duration(_) => {}
    }
}

fn inject_into_metric(m: &mut MetricExpr, id: u64) {
    // `label_filterss` is a disjunction of conjunctions
    // (`{a=...,b=...} or {c=...}`). The injection applies per
    // conjunction — each branch independently gets the
    // qualifier if it doesn't already carry one.
    if m.label_filterss.is_empty() {
        m.label_filterss.push(Vec::new());
    }
    for conjunction in m.label_filterss.iter_mut() {
        let pos = conjunction.iter().position(|f| f.label == "exec_id");
        match pos {
            Some(i) => {
                // Operator-written exec_id: resolve "latest"
                // literal but leave concrete ids alone. Any
                // other string is left as-is — the storage
                // layer will reject non-integer values.
                if conjunction[i].value == "latest" {
                    conjunction[i].value = id.to_string();
                }
            }
            None => {
                conjunction.push(LabelFilter {
                    label: "exec_id".into(),
                    op: LabelFilterOp::Eq,
                    value: id.to_string(),
                    is_template_ref: false,
                    was_quoted: false,
                    value_expr: None,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn rendered_label_filters(expr_text: &str) -> String {
        let mut e = parse(expr_text).expect("parse");
        inject_default_exec_id(&mut e, Some(7));
        crate::prettifier::pretty_print(&e)
    }

    #[test]
    fn missing_exec_id_filter_is_injected() {
        let out = rendered_label_filters(r#"cycles_total{phase="x"}"#);
        // exec_id="7" should appear, ordering is sorted by the prettifier.
        assert!(
            out.contains(r#"exec_id="7""#),
            "exec_id filter MUST be injected; got: {out}"
        );
        assert!(
            out.contains(r#"phase="x""#),
            "operator-written filter MUST be preserved; got: {out}"
        );
    }

    #[test]
    fn latest_literal_is_resolved_to_concrete_id() {
        let out = rendered_label_filters(r#"cycles_total{exec_id="latest"}"#);
        assert!(
            out.contains(r#"exec_id="7""#),
            "`latest` literal MUST resolve to the concrete id; got: {out}"
        );
        assert!(
            !out.contains(r#"exec_id="latest""#),
            "`latest` literal MUST NOT survive the rewrite; got: {out}"
        );
    }

    #[test]
    fn explicit_concrete_exec_id_is_left_alone() {
        let out = rendered_label_filters(r#"cycles_total{exec_id="3"}"#);
        assert!(
            out.contains(r#"exec_id="3""#),
            "operator-supplied concrete id MUST survive; got: {out}"
        );
        assert!(
            !out.contains(r#"exec_id="7""#),
            "rewrite MUST NOT override an explicit operator value; got: {out}"
        );
    }

    #[test]
    fn resolved_none_is_a_noop() {
        let mut e = parse(r#"cycles_total{phase="x"}"#).unwrap();
        let before = crate::prettifier::pretty_print(&e);
        inject_default_exec_id(&mut e, None);
        let after = crate::prettifier::pretty_print(&e);
        assert_eq!(
            before, after,
            "None resolver MUST leave the AST untouched (the explicit \
             aggregate-across-executions intent)"
        );
    }

    #[test]
    fn injection_walks_inside_function_calls_and_binops() {
        let out = rendered_label_filters(
            r#"rate(cycles_total{phase="x"}[1m]) / count(cycles_total{phase="x"})"#,
        );
        // Both selectors get the qualifier; count via two
        // occurrences in the rendered output.
        let count = out.matches(r#"exec_id="7""#).count();
        assert_eq!(
            count, 2,
            "every nested MetricExpr MUST get the qualifier; got: {out}"
        );
    }

    #[test]
    fn idempotent_double_injection_produces_same_ast() {
        let mut e = parse(r#"cycles_total{phase="x"}"#).unwrap();
        inject_default_exec_id(&mut e, Some(7));
        let once = crate::prettifier::pretty_print(&e);
        inject_default_exec_id(&mut e, Some(7));
        let twice = crate::prettifier::pretty_print(&e);
        assert_eq!(
            once, twice,
            "rewrite MUST be idempotent under repeated injection"
        );
    }
}
