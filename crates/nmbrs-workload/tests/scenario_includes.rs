// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Coverage for `scenario: <name>` inclusion: parser detection,
//! resolver semantics (forward refs, nesting, cycle detection),
//! and structural preservation in the parsed scenario tree.

use nmbrs_workload::model::ScenarioNode;
use nmbrs_workload::parse::parse_workload;
use std::collections::HashMap;

const PHASES: &str = r#"
phases:
  setup:
    ops: { s1: { stmt: "noop" } }
  rampup:
    ops: { r1: { stmt: "noop" } }
  search:
    ops: { q: { stmt: "noop" } }
"#;

fn nodes(yaml: &str, scenario: &str) -> Vec<ScenarioNode> {
    let wl = parse_workload(yaml, &HashMap::new())
        .unwrap_or_else(|e| panic!("parse failed: {e}\n--- yaml ---\n{yaml}"));
    wl.scenarios
        .get(scenario)
        .unwrap_or_else(|| panic!("scenario '{scenario}' not found"))
        .clone()
}

fn included(node: &ScenarioNode) -> (&str, &[ScenarioNode]) {
    match node {
        ScenarioNode::IncludedScenario { name, children } => (name.as_str(), children),
        other => panic!("expected IncludedScenario, got {other:?}"),
    }
}

#[test]
fn top_level_scenario_include_resolves_to_included_nodes() {
    let yaml = format!(
        r#"
scenarios:
  smoke:
    - setup
    - rampup
  bench:
    - scenario: smoke
    - search
{PHASES}
"#
    );
    let bench = nodes(&yaml, "bench");
    assert_eq!(bench.len(), 2, "bench should have two top-level nodes");
    let (name, kids) = included(&bench[0]);
    assert_eq!(name, "smoke");
    // The inclusion preserves structure — children are the
    // resolved nodes of `smoke`, not flattened.
    assert!(matches!(kids[0], ScenarioNode::Phase(ref n) if n == "setup"));
    assert!(matches!(kids[1], ScenarioNode::Phase(ref n) if n == "rampup"));
    // Sibling after the include stays in place.
    assert!(matches!(bench[1], ScenarioNode::Phase(ref n) if n == "search"));
}

#[test]
fn nested_scenario_include_inside_for_each() {
    let yaml = format!(
        r#"
scenarios:
  body:
    - rampup
    - search
  outer:
    - for_each: "k in 10,100"
      phases:
        - scenario: body
{PHASES}
"#
    );
    let outer = nodes(&yaml, "outer");
    assert_eq!(outer.len(), 1);
    match &outer[0] {
        ScenarioNode::Comprehension {
            comprehension,
            children,
            ..
        } => {
            assert_eq!(comprehension.coordinate_names(), vec!["k"]);
            assert_eq!(children.len(), 1);
            let (name, kids) = included(&children[0]);
            assert_eq!(name, "body");
            assert!(matches!(kids[0], ScenarioNode::Phase(ref n) if n == "rampup"));
        }
        other => panic!("expected Comprehension, got {other:?}"),
    }
}

#[test]
fn forward_reference_resolves_correctly() {
    // `bench` references `smoke` even though `smoke` appears
    // later in the YAML — resolution is order-independent.
    let yaml = format!(
        r#"
scenarios:
  bench:
    - scenario: smoke
  smoke:
    - setup
{PHASES}
"#
    );
    let bench = nodes(&yaml, "bench");
    let (name, kids) = included(&bench[0]);
    assert_eq!(name, "smoke");
    assert!(matches!(kids[0], ScenarioNode::Phase(ref n) if n == "setup"));
}

#[test]
fn unknown_scenario_name_errors_clearly() {
    let yaml = format!(
        r#"
scenarios:
  bench:
    - scenario: nope
{PHASES}
"#
    );
    let err = parse_workload(&yaml, &HashMap::new()).unwrap_err();
    assert!(err.contains("scenario include"), "got: {err}");
    assert!(err.contains("nope"), "got: {err}");
    assert!(err.contains("Known scenarios"), "got: {err}");
}

#[test]
fn cycle_is_detected_with_path() {
    // a → b → a — should error with a -> b -> a in the message.
    let yaml = format!(
        r#"
scenarios:
  a:
    - scenario: b
  b:
    - scenario: a
{PHASES}
"#
    );
    let err = parse_workload(&yaml, &HashMap::new()).unwrap_err();
    assert!(err.contains("cycle"), "expected cycle error, got: {err}");
    assert!(err.contains("a") && err.contains("b"), "got: {err}");
}

#[test]
fn deeper_cycle_is_detected() {
    // a → b → c → a
    let yaml = format!(
        r#"
scenarios:
  a:
    - scenario: b
  b:
    - scenario: c
  c:
    - scenario: a
{PHASES}
"#
    );
    let err = parse_workload(&yaml, &HashMap::new()).unwrap_err();
    assert!(err.contains("cycle"), "got: {err}");
}

#[test]
fn diamond_include_is_allowed() {
    // a includes b; b includes c; a also includes c directly.
    // Not a cycle — c is reachable along two paths but never
    // visited recursively from itself.
    let yaml = format!(
        r#"
scenarios:
  c:
    - search
  b:
    - scenario: c
  a:
    - scenario: b
    - scenario: c
{PHASES}
"#
    );
    let a = nodes(&yaml, "a");
    assert_eq!(a.len(), 2);
    assert!(matches!(a[0], ScenarioNode::IncludedScenario { ref name, .. } if name == "b"));
    assert!(matches!(a[1], ScenarioNode::IncludedScenario { ref name, .. } if name == "c"));
}

#[test]
fn include_inside_phases_list_alongside_phase_strings() {
    // Mixed list — bare phase references and an include side
    // by side under a `phases:` key. The user's exact sketch.
    let yaml = format!(
        r#"
scenarios:
  inner:
    - search
  outer:
    - for_each: "x in 1,2"
      phases:
        - setup
        - scenario: inner
        - rampup
{PHASES}
"#
    );
    let outer = nodes(&yaml, "outer");
    let ScenarioNode::Comprehension { children, .. } = &outer[0] else {
        panic!("expected Comprehension");
    };
    assert_eq!(children.len(), 3);
    assert!(matches!(children[0], ScenarioNode::Phase(ref n) if n == "setup"));
    assert!(
        matches!(children[1], ScenarioNode::IncludedScenario { ref name, .. } if name == "inner")
    );
    assert!(matches!(children[2], ScenarioNode::Phase(ref n) if n == "rampup"));
}
