// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-108 equivalence proof: composing `vector_suite_blueprint`
//! with `cql/vector_suite/vector_suite_cql_impl` must yield EXACTLY the
//! workload model of the direct-bound monolith
//! (`cql/vector_suite/vector_suite_cql_direct`) for every suite scenario —
//! same scenario trees, same phase scaffolding, same op bodies,
//! same effective params. The model level is where synthesized
//! programs come from, so model equality is result equivalence
//! against any given target.
//!
//! Comparison is DEEP serde equality with a narrow, documented
//! set of normalizations — each either provably non-semantic or
//! self-verifying:
//! - the pair's `connect` phase (SRD-109 session establishment —
//!   a one-op liveness leg with no measurement role) is stripped
//!   from its scenario trees and skipped in phase comparison;
//! - binding sources compare comment/blank-line-insensitively
//!   (comments are documentation; the GK parser ignores them);
//! - string leaves compare with per-line right-trim (YAML block
//!   scalar chomping noise, semantically inert in statements);
//! - composition plumbing (`abstract_interface`,
//!   `interface_bound`, op `description`) is ignored;
//! - a param declared by only ONE side must be textually
//!   unreferenced (word-boundary) in every compared phase and
//!   scenario of BOTH sides — proving it cannot affect compared
//!   behavior; params declared by BOTH sides must be value-equal
//!   with no exemptions. Root-binding wires get the same
//!   one-sided rule.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde_json::Value;

const SUITE_SCENARIOS: &[&str] = &[
    "traverse",
    "capacity",
    "load_build",
    "search_perf",
    "filtered_grid",
    "streaming",
    "cold_warm",
    "churn",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("nmbrs crate has a parent dir")
        .to_path_buf()
}

fn load(path: &Path) -> nmbrs_workload::model::Workload {
    let (merged, warnings) = nmbrs_workload::extends::load_and_merge(path)
        .unwrap_or_else(|e| panic!("load {}: {e}", path.display()));
    assert!(
        warnings.is_empty(),
        "suite files must resolve unambiguously: {warnings:?}"
    );
    nmbrs_workload::parse::parse_workload(&merged, &HashMap::new())
        .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// The direct monolith, extends-resolved.
fn direct() -> nmbrs_workload::model::Workload {
    load(&repo_root().join("nmbrs/workloads/cql/vector_suite/vector_suite_cql_direct.yaml"))
}

/// The blueprint with the CQL implementation bound in.
fn pair() -> nmbrs_workload::model::Workload {
    let mut blueprint = load(&repo_root().join("nmbrs/workloads/vector_suite_blueprint.yaml"));
    let implementation =
        load(&repo_root().join("nmbrs/workloads/cql/vector_suite/vector_suite_cql_impl.yaml"));
    nmbrs_workload::implements::bind_implementation(&mut blueprint, implementation)
        .expect("bind vector_suite_cql_impl into vector_suite_blueprint");
    assert!(
        nmbrs_workload::implements::unbound_abstract_slots(&blueprint).is_empty(),
        "every abstract slot must be bound"
    );
    blueprint
}

/// Strip `- connect` phase nodes anywhere in a scenario tree.
fn strip_connect(v: Value) -> Value {
    match v {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .filter(|item| {
                    !matches!(
                        item,
                        Value::Object(m) if m.len() == 1
                            && m.get("Phase").and_then(Value::as_str) == Some("connect")
                    )
                })
                .map(strip_connect)
                .collect(),
        ),
        Value::Object(m) => {
            Value::Object(m.into_iter().map(|(k, v)| (k, strip_connect(v))).collect())
        }
        other => other,
    }
}

/// Collect every `Phase` name referenced anywhere in a tree.
fn collect_phases(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::Array(items) => items.iter().for_each(|i| collect_phases(i, out)),
        Value::Object(m) => {
            if let Some(Value::String(name)) = m.get("Phase") {
                if m.len() == 1 {
                    out.insert(name.clone());
                }
            }
            m.values().for_each(|v| collect_phases(v, out));
        }
        _ => {}
    }
}

/// Comment/blank-insensitive normalization for GK binding sources.
fn normalize_bindings_source(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Per-line right-trim for every other string leaf (YAML block
/// scalar chomping noise).
fn rtrim_lines(s: &str) -> String {
    s.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

/// Normalize a serialized phase for comparison: drop composition
/// plumbing, normalize binding sources, right-trim string leaves.
fn normalize(v: Value, in_bindings: bool) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .filter(|(k, _)| {
                    !matches!(
                        k.as_str(),
                        "abstract_interface" | "interface_bound" | "description"
                    )
                })
                .map(|(k, v)| {
                    let is_bindings = k == "bindings" || k == "PolydatSource";
                    (k, normalize(v, is_bindings))
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|i| normalize(i, in_bindings))
                .collect(),
        ),
        Value::String(s) => Value::String(if in_bindings {
            normalize_bindings_source(&s)
        } else {
            rtrim_lines(&s)
        }),
        other => other,
    }
}

/// Scrub workload-declared params off op `params` maps: the
/// parser copies the whole workload params map onto every op,
/// with raw YAML scalar types — data the root-level params rule
/// already compares (as strings, with the one-sided exemption
/// proof). What remains after the scrub is genuine op-level
/// param matter, compared strictly with scalars stringified.
fn scrub_op_params(v: &mut Value, declared: &BTreeSet<String>) {
    let Value::Object(m) = v else { return };
    let Some(Value::Array(ops)) = m.get_mut("ops") else {
        return;
    };
    for op in ops {
        let Value::Object(om) = op else { continue };
        let Some(Value::Object(pm)) = om.get_mut("params") else {
            continue;
        };
        pm.retain(|k, _| !declared.contains(k));
        for (_, pv) in pm.iter_mut() {
            match pv {
                Value::Number(n) => *pv = Value::String(n.to_string()),
                Value::Bool(b) => *pv = Value::String(b.to_string()),
                _ => {}
            }
        }
    }
}

/// Word-boundary occurrence test: `name` appears in `text` with
/// no identifier character on either side.
fn word_referenced(text: &str, name: &str) -> bool {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(pos) = text[from..].find(name) {
        let start = from + pos;
        let end = start + name.len();
        let before_ok = start == 0 || !is_word(bytes[start - 1] as char);
        let after_ok = end == bytes.len() || !is_word(bytes[end] as char);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// LHS wire names of a root `bindings:` source.
fn root_binding_wires(w: &nmbrs_workload::model::Workload) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let nmbrs_workload::model::BindingsDef::PolydatSource(src) = &w.bindings {
        for line in src.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((lhs, _)) = line.split_once(":=").or_else(|| line.split_once('=')) {
                let name = lhs
                    .trim()
                    .rsplit(|c: char| c.is_whitespace())
                    .next()
                    .unwrap_or("")
                    .to_string();
                if !name.is_empty() {
                    out.insert(name);
                }
            }
        }
    }
    out
}

/// Everything compared, flattened to text — the reference corpus
/// for the self-verifying one-sided exemptions.
fn compared_content(
    direct: &nmbrs_workload::model::Workload,
    pair: &nmbrs_workload::model::Workload,
    reachable: &BTreeSet<String>,
    declared: &BTreeSet<String>,
) -> String {
    let mut text = String::new();
    for w in [direct, pair] {
        for name in SUITE_SCENARIOS {
            if let Some(tree) = w.scenarios.get(*name) {
                text.push_str(&serde_json::to_value(tree).unwrap().to_string());
            }
        }
        for name in reachable {
            if let Some(phase) = w.phases.get(name) {
                let mut v = serde_json::to_value(phase).unwrap();
                scrub_op_params(&mut v, declared);
                text.push_str(&normalize(v, false).to_string());
            }
        }
    }
    text
}

#[test]
fn suite_model_equivalence() {
    let direct = direct();
    let pair = pair();

    // ── Scenario trees: identical modulo the pair's `connect`. ──
    let mut reachable = BTreeSet::new();
    for name in SUITE_SCENARIOS {
        let d_tree = direct
            .scenarios
            .get(*name)
            .unwrap_or_else(|| panic!("direct lacks scenario '{name}'"));
        let p_tree = pair
            .scenarios
            .get(*name)
            .unwrap_or_else(|| panic!("pair lacks scenario '{name}'"));
        let d_v = serde_json::to_value(d_tree).unwrap();
        let p_v = strip_connect(serde_json::to_value(p_tree).unwrap());
        assert_eq!(
            d_v, p_v,
            "scenario '{name}' diverges (pair compared with `connect` stripped)\n\
             direct: {d_v:#}\npair:   {p_v:#}"
        );
        collect_phases(&d_v, &mut reachable);
    }
    assert!(
        !reachable.contains("connect"),
        "the direct monolith must not carry a connect phase (it is the pair's addition)"
    );

    // ── Phases: deep equality for every phase the suite walks. ──
    let declared: BTreeSet<String> = direct
        .declared_params
        .iter()
        .chain(pair.declared_params.iter())
        .cloned()
        .collect();
    for name in &reachable {
        let d_phase = direct
            .phases
            .get(name)
            .unwrap_or_else(|| panic!("direct lacks phase '{name}'"));
        let p_phase = pair
            .phases
            .get(name)
            .unwrap_or_else(|| panic!("pair lacks phase '{name}'"));
        let mut d_v = serde_json::to_value(d_phase).unwrap();
        let mut p_v = serde_json::to_value(p_phase).unwrap();
        scrub_op_params(&mut d_v, &declared);
        scrub_op_params(&mut p_v, &declared);
        let d_v = normalize(d_v, false);
        let p_v = normalize(p_v, false);
        assert_eq!(
            d_v, p_v,
            "phase '{name}' diverges\ndirect: {d_v:#}\npair:   {p_v:#}"
        );
    }

    // The blueprint carries no dead phases: its phase set is
    // exactly the reachable set plus `connect`.
    let mut expected: BTreeSet<String> = reachable.clone();
    expected.insert("connect".to_string());
    let pair_phases: BTreeSet<String> = pair.phases.keys().cloned().collect();
    assert_eq!(
        expected, pair_phases,
        "blueprint phases must be exactly the suite's reachable set + connect"
    );

    // ── Params: both-sided equal; one-sided provably unconsumed. ──
    let content = compared_content(&direct, &pair, &reachable, &declared);
    let d_declared: BTreeSet<&String> = direct.declared_params.iter().collect();
    let p_declared: BTreeSet<&String> = pair.declared_params.iter().collect();
    for name in d_declared.union(&p_declared) {
        match (d_declared.contains(*name), p_declared.contains(*name)) {
            (true, true) => assert_eq!(
                direct.params.get(*name),
                pair.params.get(*name),
                "param '{name}' differs between direct and pair"
            ),
            _ => assert!(
                !word_referenced(&content, name),
                "param '{name}' is declared by one side only, yet the compared \
                 content references it — declare it on both sides or remove the use"
            ),
        }
    }

    // ── Root bindings: one-sided wires must be unconsumed. ──
    let d_wires = root_binding_wires(&direct);
    let p_wires = root_binding_wires(&pair);
    for wire in d_wires.symmetric_difference(&p_wires) {
        assert!(
            !word_referenced(&content, wire),
            "root binding wire '{wire}' exists on one side only, yet the \
             compared content references it"
        );
    }

    // ── Root scaffolding. ──
    assert_eq!(direct.stick_session, pair.stick_session, "stick_session");
    assert_eq!(
        direct.status_metrics, pair.status_metrics,
        "root status_metrics"
    );
    assert!(
        direct.stop_when.is_empty() && pair.stop_when.is_empty(),
        "root stop_when"
    );
    assert!(
        direct.ops.is_empty() && pair.ops.is_empty(),
        "top-level ops"
    );
    assert!(
        direct.wrappers.is_none() && pair.wrappers.is_none(),
        "root wrappers"
    );

    // ── Report: defaults + shared groups equal; one-sided groups
    // must not touch any suite phase. ──
    let d_report = serde_json::to_value(&direct.report).unwrap();
    let p_report = serde_json::to_value(&pair.report).unwrap();
    assert_eq!(
        d_report["defaults"], p_report["defaults"],
        "report defaults"
    );
    let group_names = |r: &Value| -> BTreeSet<String> {
        r["groups"]
            .as_array()
            .map(|groups| {
                groups
                    .iter()
                    .filter_map(|g| g["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let find_group = |r: &Value, name: &str| -> Value {
        r["groups"]
            .as_array()
            .and_then(|groups| groups.iter().find(|g| g["name"] == name))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let d_groups = group_names(&d_report);
    let p_groups = group_names(&p_report);
    for name in d_groups.intersection(&p_groups) {
        assert_eq!(
            normalize(find_group(&d_report, name), false),
            normalize(find_group(&p_report, name), false),
            "report group '{name}' diverges"
        );
    }
    for name in d_groups.symmetric_difference(&p_groups) {
        let text =
            find_group(&d_report, name).to_string() + &find_group(&p_report, name).to_string();
        for phase in expected.iter() {
            assert!(
                !word_referenced(&text, phase),
                "one-sided report group '{name}' references suite phase '{phase}'"
            );
        }
    }
}

/// The composed pair must not reference the direct monolith: the
/// blueprint and its implementation stand alone.
#[test]
fn pair_documents_do_not_mention_the_direct_form() {
    let root = repo_root();
    for rel in [
        "nmbrs/workloads/vector_suite_blueprint.yaml",
        "nmbrs/workloads/cql/vector_suite/vector_suite_cql_impl.yaml",
        "nmbrs/workloads/cql/vector_suite/vector_suite_cql_oss.yaml",
        "nmbrs/workloads/cql/vector_suite/vector_suite_cql_oss_sift128.yaml",
    ] {
        let text =
            std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        assert!(
            !text.contains("vector_suite_cql_direct"),
            "{rel} mentions vector_suite_cql_direct"
        );
    }
}
