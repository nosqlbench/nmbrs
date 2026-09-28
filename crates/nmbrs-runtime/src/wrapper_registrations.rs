// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-32a — Cross-cutting helpers for wrapper registrations.
//!
//! Each wrapper now declares its own activation field, trigger
//! predicate, ordering constraints, and `inventory::submit!`
//! block inside the wrapper's own source file (the modules
//! under [`crate::wrappers`], plus [`crate::validation`] for
//! the validate layer). This module retains only:
//!
//! - Small text-format helpers (`trim_braces`, `short_value`,
//!   `json_to_u64`) shared by several `describe_assignment`
//!   implementations.
//! - The integration tests that exercise the merged registry
//!   after every wrapper has submitted itself via `inventory`.
//!
//! The constants previously declared here (`TRAVERSE`,
//! `THROTTLE`, …) moved to their respective wrappers as
//! `pub const NAME: WrapperName`. Code that needs a particular
//! wrapper's name reaches it via, e.g.,
//! `crate::wrappers::delay::NAME` (and
//! `crate::validation::WRAPPER_NAME` for the validate layer).

/// Strip the brace wrapping from a substitution-style binding
/// reference (e.g. `"{rate}"` → `"rate"`). Leaves any other
/// string unchanged.
pub(crate) fn trim_braces(s: &str) -> &str {
    let t = s.trim();
    t.strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or(t)
}

/// Best-effort u64 extraction from a JSON value: accepts a
/// numeric scalar or a string-encoded integer. Used by the
/// describe paths for wrappers whose config can come in either
/// form (`poll: { interval_ms: 5000 }` vs `poll: { interval_ms: "5000" }`).
pub(crate) fn json_to_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
}

/// Render a JSON value as a short, single-line string for
/// init-time diagnostics. Long objects/arrays are abbreviated.
pub(crate) fn short_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => format!("\"{s}\""),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            let s = v.to_string();
            if s.len() > 60 {
                format!("{}…", &s[..60])
            } else {
                s
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::wrapper_registry::{WrapperRegistry, WrapperSubject};
    use crate::wrapper_resolver::{WrapperActivation, WrapperResolver};
    use nmbrs_workload::model::ParsedOp;

    fn empty_template(name: &str) -> ParsedOp {
        ParsedOp::simple(name, "")
    }

    #[test]
    fn registry_collects_all_eight_wrappers() {
        let r = WrapperRegistry::from_inventory();
        let names: Vec<&str> = r.iter().map(|reg| reg.name.as_str()).collect();
        for expected in [
            "traverse", "delay", "validate", "poll", "if", "fields", "result", "metrics",
        ] {
            assert!(
                names.contains(&expected),
                "registry missing `{expected}`; got {names:?}",
            );
        }
    }

    #[test]
    fn default_order_passes_constraint_validation() {
        let r = WrapperRegistry::from_inventory();
        let resolver = WrapperResolver::with_default_order(&r);
        assert!(
            resolver.is_ok(),
            "default order should validate: {:?}",
            resolver.err()
        );
    }

    #[test]
    fn empty_template_resolves_to_always_on_set() {
        let r = WrapperRegistry::from_inventory();
        let resolver = WrapperResolver::with_default_order(&r).unwrap();
        let plan = resolver
            .resolve(WrapperSubject::Op(&empty_template("noop")), &r)
            .unwrap();
        let names: Vec<&str> = plan.stack.iter().map(|reg| reg.name.as_str()).collect();
        // The three always-on wrappers in innermost→outermost order:
        // traverse (innermost; reads body to count rows / extract
        // captures), result (always-on; no-op when result map is
        // empty), and errors (SRD-82 Part 3b — every op has an
        // effective error policy; the wrapper is the outermost
        // terminal-outcome observer). MEMO / METRICS / DRYRUN are all
        // conditional and don't fire on an empty template. DRYRUN
        // specifically triggers on the injected `dryrun:` template
        // parameter which an empty template does not carry.
        assert_eq!(names, vec!["traverse", "result", "errors"]);
    }

    #[test]
    fn dryrun_marker_activates_dryrun_wrapper() {
        let r = WrapperRegistry::from_inventory();
        let resolver = WrapperResolver::with_default_order(&r).unwrap();
        let mut t = empty_template("noop");
        t.params
            .insert("dryrun".into(), serde_json::Value::String("silent".into()));
        let plan = resolver.resolve(WrapperSubject::Op(&t), &r).unwrap();
        let names: Vec<&str> = plan.stack.iter().map(|reg| reg.name.as_str()).collect();
        // `errors` sits outside even dryrun (it is inert on the
        // short-circuit's Ok) — dryrun is the outermost DISPATCHED
        // layer; the errors slot is the hand-placed terminal observer.
        assert_eq!(
            names,
            vec!["traverse", "result", "dryrun", "errors"],
            "dryrun marker present → dryrun wrapper in plan"
        );
    }

    #[test]
    fn validate_pulls_in_traverse_transitively() {
        let r = WrapperRegistry::from_inventory();
        let resolver = WrapperResolver::with_default_order(&r).unwrap();
        let mut t = empty_template("v_op");
        t.params.insert(
            "verify".into(),
            serde_json::Value::String("min_rows >= 1".into()),
        );
        let plan = resolver.resolve(WrapperSubject::Op(&t), &r).unwrap();

        // Traverse must be present and must precede validate.
        let names: Vec<&str> = plan.stack.iter().map(|reg| reg.name.as_str()).collect();
        let i_traverse = names.iter().position(|n| *n == "traverse").unwrap();
        let i_validate = names.iter().position(|n| *n == "validate").unwrap();
        assert!(
            i_traverse < i_validate,
            "traverse must be inside validate: {names:?}"
        );

        // Activation provenance distinguishes triggered vs transitive.
        let validate_act = plan.activation(crate::validation::WRAPPER_NAME).unwrap();
        assert!(
            matches!(
                validate_act,
                WrapperActivation::OwnedField {
                    field: "verify",
                    ..
                }
            ),
            "validate should be OwnedField(verify): {validate_act:?}"
        );
    }

    #[test]
    fn override_must_include_every_triggered_wrapper() {
        let r = WrapperRegistry::from_inventory();
        let resolver = WrapperResolver::with_default_order(&r).unwrap();
        let mut t = empty_template("v_op");
        t.params.insert(
            "verify".into(),
            serde_json::Value::String("min_rows >= 1".into()),
        );
        match resolver.resolve_with_order(WrapperSubject::Op(&t), &r, &["validate", "result"]) {
            Err(crate::wrapper_resolver::ResolveError::OverridePermutationMismatch {
                missing: Some(_),
                ..
            }) => {}
            Err(other) => panic!("expected missing-wrapper error; got {other:?}"),
            Ok(_) => panic!("override missing a triggered wrapper must error"),
        }
    }

    #[test]
    fn override_must_not_include_non_triggered_wrappers() {
        let r = WrapperRegistry::from_inventory();
        let resolver = WrapperResolver::with_default_order(&r).unwrap();
        let t = empty_template("noop");
        match resolver.resolve_with_order(
            WrapperSubject::Op(&t),
            &r,
            &["traverse", "poll", "result", "errors"],
        ) {
            Err(crate::wrapper_resolver::ResolveError::OverridePermutationMismatch {
                extra: Some(_),
                ..
            }) => {}
            Err(other) => panic!("expected extra-wrapper error; got {other:?}"),
            Ok(_) => panic!("override naming a non-triggered wrapper must error"),
        }
    }

    #[test]
    fn override_with_unknown_name_suggests_typo() {
        let r = WrapperRegistry::from_inventory();
        let resolver = WrapperResolver::with_default_order(&r).unwrap();
        let t = empty_template("noop");
        match resolver.resolve_with_order(
            WrapperSubject::Op(&t),
            &r,
            &["traverse", "validatte", "result"],
        ) {
            Err(crate::wrapper_resolver::ResolveError::UnknownWrapper { name, suggestion }) => {
                assert_eq!(name, "validatte");
                assert_eq!(suggestion, Some("validate"));
            }
            Err(other) => panic!("expected UnknownWrapper; got {other:?}"),
            Ok(_) => panic!("unknown name must error"),
        }
    }

    #[test]
    fn cli_default_order_replaces_built_in_tiebreaker() {
        let r = WrapperRegistry::from_inventory();
        let custom = vec![
            "traverse", "delay", "validate", "if", "poll", "fields", "result", "metrics",
        ];
        let resolver = WrapperResolver::from_names(&custom, &r).unwrap();
        let mut t = empty_template("flexible");
        t.params.insert(
            "poll".into(),
            serde_json::Value::String("await_empty".into()),
        );
        t.condition = Some("flag".into());
        let plan = resolver.resolve(WrapperSubject::Op(&t), &r).unwrap();
        let names: Vec<&str> = plan.stack.iter().map(|reg| reg.name.as_str()).collect();
        let i_if = names.iter().position(|n| *n == "if").unwrap();
        let i_poll = names.iter().position(|n| *n == "poll").unwrap();
        assert!(
            i_if < i_poll,
            "custom default should place if INSIDE poll: {names:?}"
        );
    }

    #[test]
    fn override_can_invert_default_tiebreaker_order() {
        let r = WrapperRegistry::from_inventory();
        let resolver = WrapperResolver::with_default_order(&r).unwrap();
        let mut t = empty_template("flexible");
        t.params
            .insert("verify".into(), serde_json::Value::String("ok".into()));
        t.params.insert(
            "poll".into(),
            serde_json::Value::String("await_empty".into()),
        );
        let plan = resolver
            .resolve_with_order(
                WrapperSubject::Op(&t),
                &r,
                &["traverse", "poll", "validate", "result", "errors"],
            )
            .unwrap();
        let names: Vec<&str> = plan.stack.iter().map(|reg| reg.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["traverse", "poll", "validate", "result", "errors"]
        );
    }

    #[test]
    fn strict_without_verify_is_misplaced() {
        let r = WrapperRegistry::from_inventory();
        let mut t = empty_template("noop");
        t.params
            .insert("strict".into(), serde_json::Value::Bool(true));
        let violations = r.misplaced_fields(WrapperSubject::Op(&t));
        let names: Vec<(&str, &str)> = violations.iter().map(|(w, f)| (w.as_str(), *f)).collect();
        assert!(
            names.contains(&("validate", "strict")),
            "strict alone must be misplaced; got {names:?}"
        );
    }

    #[test]
    fn poll_as_map_triggers_wrapper() {
        let r = WrapperRegistry::from_inventory();
        let mut t = empty_template("polled");
        let mut cfg = serde_json::Map::new();
        cfg.insert(
            "mode".into(),
            serde_json::Value::String("await_empty".into()),
        );
        cfg.insert("interval_ms".into(), serde_json::Value::Number(5000.into()));
        cfg.insert(
            "timeout_ms".into(),
            serde_json::Value::Number(600_000.into()),
        );
        t.params
            .insert("poll".into(), serde_json::Value::Object(cfg));
        // poll: <map> triggers the wrapper through the registry.
        let resolver = WrapperResolver::with_default_order(&r).unwrap();
        let plan = resolver.resolve(WrapperSubject::Op(&t), &r).unwrap();
        let names: Vec<&str> = plan.stack.iter().map(|reg| reg.name.as_str()).collect();
        assert!(
            names.contains(&"poll"),
            "poll wrapper should activate: {names:?}"
        );
        // And `poll:` is the wrapper's only owned field, so it's
        // never misplaced.
        let violations = r.misplaced_fields(WrapperSubject::Op(&t));
        assert!(violations.is_empty(), "got {violations:?}");
    }

    #[test]
    fn full_workload_default_order_matches_cascade() {
        let r = WrapperRegistry::from_inventory();
        let resolver = WrapperResolver::with_default_order(&r).unwrap();
        let mut t = empty_template("full");
        t.delay = Some(nmbrs_workload::model::DelaySpec::Before("rate".into()));
        t.params
            .insert("verify".into(), serde_json::Value::String("ok".into()));
        t.params.insert(
            "poll".into(),
            serde_json::Value::String("await_empty".into()),
        );
        t.condition = Some("flag".into());
        t.params
            .insert("fields".into(), serde_json::Value::Bool(true));
        t.metrics.insert(
            "recall".into(),
            nmbrs_workload::model::MetricSpec {
                cell: Default::default(),
                value: "recall_value".into(),
                family: None,
                kind: None,
                unit: None,
                format: None,
            },
        );
        let plan = resolver.resolve(WrapperSubject::Op(&t), &r).unwrap();
        let names: Vec<&str> = plan.stack.iter().map(|reg| reg.name.as_str()).collect();
        // `fields` sits outer of dryrun to support `dryrun=fields`
        // semantics — the pre-execute render must fire BEFORE
        // DRYRUN's short-circuit; `errors` (SRD-82 Part 3b) is the
        // absolute outermost terminal-outcome observer. With no
        // `dryrun:` injected on this op the dispatched stack ends at
        // `fields`, with the errors slot outside it.
        assert_eq!(
            names,
            vec![
                "traverse", "delay", "validate", "poll", "if", "result", "metrics", "fields",
                "errors",
            ]
        );
    }

    /// Regression: when an op carries the injected `dryrun:`
    /// marker AND a `memo:` declaration, MEMO must be placed
    /// INSIDE DRYRUN.
    #[test]
    fn memo_sits_inside_dryrun_when_both_active() {
        let r = WrapperRegistry::from_inventory();
        let resolver = WrapperResolver::with_default_order(&r).unwrap();
        let mut t = empty_template("drop_keyspace");
        t.params.insert(
            "memo".into(),
            serde_json::Value::String("dropping keyspace".into()),
        );
        t.params
            .insert("dryrun".into(), serde_json::Value::String("silent".into()));
        let plan = resolver.resolve(WrapperSubject::Op(&t), &r).unwrap();
        let names: Vec<&str> = plan.stack.iter().map(|reg| reg.name.as_str()).collect();
        let i_memo = names
            .iter()
            .position(|n| *n == "memo")
            .expect("memo triggered by memo: param");
        let i_dryrun = names
            .iter()
            .position(|n| *n == "dryrun")
            .expect("dryrun triggered by injected dryrun: param");
        assert!(
            i_memo < i_dryrun,
            "memo must be inside dryrun in default order; got {names:?}"
        );
    }

    /// Variant: when EVERY wrapper activates (full template
    /// plus injected dryrun marker), DRYRUN sits just below
    /// `fields` — the latter is intentionally allowed outer of
    /// dryrun so `dryrun=fields`'s pre-execute render fires
    /// before DRYRUN's short-circuit. Every other wrapper
    /// stays inside DRYRUN.
    #[test]
    fn dryrun_is_outermost_with_full_wrapper_set() {
        let r = WrapperRegistry::from_inventory();
        let resolver = WrapperResolver::with_default_order(&r).unwrap();
        let mut t = empty_template("full");
        t.delay = Some(nmbrs_workload::model::DelaySpec::Before("rate".into()));
        t.params
            .insert("verify".into(), serde_json::Value::String("ok".into()));
        t.params.insert(
            "poll".into(),
            serde_json::Value::String("await_empty".into()),
        );
        t.condition = Some("flag".into());
        t.params
            .insert("fields".into(), serde_json::Value::Bool(true));
        t.params
            .insert("memo".into(), serde_json::Value::String("doing X".into()));
        t.params
            .insert("dryrun".into(), serde_json::Value::String("silent".into()));
        t.metrics.insert(
            "recall".into(),
            nmbrs_workload::model::MetricSpec {
                cell: Default::default(),
                value: "recall_value".into(),
                family: None,
                kind: None,
                unit: None,
                format: None,
            },
        );
        let plan = resolver.resolve(WrapperSubject::Op(&t), &r).unwrap();
        let names: Vec<&str> = plan.stack.iter().map(|reg| reg.name.as_str()).collect();
        // `errors` (SRD-82 Part 3b) is the absolute outermost slot —
        // inert on dryrun's Ok short-circuit; `fields` sits outer of
        // dryrun so its pre-execute render fires before the
        // short-circuit; `dryrun` is third-outermost. Every other
        // wrapper is inside DRYRUN's short-circuit.
        assert_eq!(
            *names.last().unwrap(),
            "errors",
            "errors must be the absolute outermost slot; got {names:?}"
        );
        assert_eq!(
            names[names.len() - 2],
            "fields",
            "fields must sit just below errors — its pre-execute render \
             must fire before DRYRUN's short-circuit; got {names:?}"
        );
        let dryrun_idx = names
            .iter()
            .position(|n| *n == "dryrun")
            .expect("dryrun triggered by injected dryrun: param");
        assert_eq!(
            dryrun_idx,
            names.len() - 3,
            "dryrun must sit just below fields; got {names:?}"
        );
        // Every wrapper inner of dryrun is short-circuited
        // — assert the non-fields/non-dryrun/non-errors set lives
        // strictly inside dryrun.
        for n in &names {
            if *n == "fields" || *n == "dryrun" || *n == "errors" {
                continue;
            }
            let i = names.iter().position(|x| x == n).unwrap();
            assert!(
                i < dryrun_idx,
                "{n} must sit inside dryrun's short-circuit; got {names:?}"
            );
        }
    }
}
