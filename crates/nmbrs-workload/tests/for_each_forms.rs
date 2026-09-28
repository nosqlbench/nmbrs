// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Coverage for the six syntactic shapes of `for_each` and the
//! detection rule that picks between Cartesian (single or
//! multi-clause) and Union comprehension modes.
//!
//! Assertions walk the algebra AST directly: a `for_each:`
//! parses to a `polydat::iteration::comprehension::Comprehension`
//! whose top-level shape (Clause / Cartesian / Union / Zip,
//! optionally wrapped by Filter / Order) records the
//! parsed-out structure.

use nmbrs_workload::model::ScenarioNode;
use nmbrs_workload::parse::parse_workload;
use polydat::iteration::comprehension::Comprehension as Algebra;
use polydat::iteration::comprehension::source::{LiteralValue, Source};
use polydat::iteration::comprehension::strategy::ZipMode;
use std::collections::HashMap;

fn first_scenario_node(yaml: &str) -> ScenarioNode {
    let wl = parse_workload(yaml, &HashMap::new())
        .unwrap_or_else(|e| panic!("parse failed: {e}\n--- yaml ---\n{yaml}"));
    let nodes = wl.scenarios.values().next().expect("at least one scenario");
    nodes.first().cloned().expect("at least one scenario node")
}

/// Peel outer `Order` / `Filter` wrappers; return the
/// structural body. Tests that care about modifiers assert
/// on the modifier separately.
fn body_of(comp: &Algebra) -> &Algebra {
    let mut body = comp;
    loop {
        match body {
            Algebra::Order { child, .. } | Algebra::Filter { child, .. } => body = child,
            _ => break body,
        }
    }
}

/// Optional filter predicate, if the comprehension is wrapped
/// in (or below an Order wrapping) a Filter.
fn filter_of(comp: &Algebra) -> Option<&str> {
    let mut node = comp;
    loop {
        match node {
            Algebra::Order { child, .. } => node = child,
            Algebra::Filter { predicate, .. } => return Some(predicate.as_str()),
            _ => return None,
        }
    }
}

/// Get a leaf Clause's source if `node` is one.
fn clause_source(node: &Algebra) -> Option<&Source> {
    match node {
        Algebra::Clause { source, .. } => Some(source),
        _ => None,
    }
}

/// Get a leaf Clause's name if `node` is one.
fn clause_name(node: &Algebra) -> Option<&str> {
    match node {
        Algebra::Clause { name, .. } => Some(name.as_str()),
        _ => None,
    }
}

/// Extract Literal values from a source, panicking otherwise.
fn literal_values(source: &Source) -> &[LiteralValue] {
    match source {
        Source::Literal { values } => values.as_slice(),
        other => panic!("expected Literal source, got {other:?}"),
    }
}

const PHASES: &str = r#"
phases:
  p:
    ops:
      step:
        stmt: "noop"
"#;

#[test]
fn string_single_clause_is_single_clause_cartesian() {
    let yaml = format!(
        r#"
scenarios:
  s:
    - for_each: "x in 1,2,3"
      phases: [p]
{PHASES}
"#
    );
    let node = first_scenario_node(&yaml);
    let ScenarioNode::Comprehension { comprehension, .. } = node else {
        panic!("expected Comprehension");
    };
    // Single-clause Cartesian collapses to bare Clause at the algebra layer.
    let body = body_of(&comprehension);
    assert_eq!(clause_name(body), Some("x"));
    let values = literal_values(clause_source(body).unwrap());
    assert_eq!(values.len(), 3);
}

#[test]
fn string_multi_clause_distinct_vars_is_multi_clause_cartesian() {
    let yaml = format!(
        r#"
scenarios:
  s:
    - for_each: "x in 1,2, y in a,b"
      phases: [p]
{PHASES}
"#
    );
    let node = first_scenario_node(&yaml);
    let ScenarioNode::Comprehension { comprehension, .. } = node else {
        panic!("expected Comprehension");
    };
    let body = body_of(&comprehension);
    match body {
        Algebra::Cartesian { children } => {
            assert_eq!(children.len(), 2);
            assert_eq!(clause_name(&children[0]), Some("x"));
            assert_eq!(clause_name(&children[1]), Some("y"));
        }
        other => panic!("expected Cartesian, got {other:?}"),
    }
}

#[test]
fn string_multi_clause_repeated_var_is_union() {
    // Same var name twice in a single string ⇒ Union (each
    // clause is its own sub-space).
    let yaml = format!(
        r#"
scenarios:
  s:
    - for_each: "x in 1, x in 2"
      phases: [p]
{PHASES}
"#
    );
    let node = first_scenario_node(&yaml);
    let ScenarioNode::Comprehension { comprehension, .. } = node else {
        panic!("expected Comprehension");
    };
    let body = body_of(&comprehension);
    match body {
        Algebra::Union { children } => {
            assert_eq!(children.len(), 2);
            assert_eq!(clause_name(&children[0]), Some("x"));
            assert_eq!(clause_name(&children[1]), Some("x"));
            let v0 = literal_values(clause_source(&children[0]).unwrap());
            assert_eq!(v0, &[LiteralValue::Int(1)]);
            let v1 = literal_values(clause_source(&children[1]).unwrap());
            assert_eq!(v1, &[LiteralValue::Int(2)]);
        }
        other => panic!("expected Union, got {other:?}"),
    }
}

#[test]
fn array_single_clause_distinct_vars_is_multi_clause_cartesian() {
    let yaml = format!(
        r#"
scenarios:
  s:
    - for_each:
        - "x in 1,2"
        - "y in a,b"
      phases: [p]
{PHASES}
"#
    );
    let node = first_scenario_node(&yaml);
    let ScenarioNode::Comprehension { comprehension, .. } = node else {
        panic!("expected Comprehension");
    };
    let body = body_of(&comprehension);
    match body {
        Algebra::Cartesian { children } => {
            assert_eq!(children.len(), 2);
            assert_eq!(clause_name(&children[0]), Some("x"));
            assert_eq!(clause_name(&children[1]), Some("y"));
        }
        other => panic!("expected Cartesian, got {other:?}"),
    }
}

#[test]
fn array_single_clause_repeated_var_is_union() {
    // Two array entries each with one clause, same var name ⇒
    // Union of single-var sub-spaces.
    let yaml = format!(
        r#"
scenarios:
  s:
    - for_each:
        - "x in 1"
        - "x in 2"
      phases: [p]
{PHASES}
"#
    );
    let node = first_scenario_node(&yaml);
    let ScenarioNode::Comprehension { comprehension, .. } = node else {
        panic!("expected Comprehension");
    };
    let body = body_of(&comprehension);
    match body {
        Algebra::Union { children } => {
            assert_eq!(children.len(), 2);
        }
        other => panic!("expected Union, got {other:?}"),
    }
}

#[test]
fn array_multi_clause_repeated_vars_is_union() {
    // The motivating union shape: two array entries, each is a
    // multi-dim sub-space; vars repeat across entries.
    let yaml = format!(
        r#"
scenarios:
  s:
    - for_each:
        - "k in 10, limit in 10,20,30"
        - "k in 100, limit in 100,200,300"
      phases: [p]
{PHASES}
"#
    );
    let node = first_scenario_node(&yaml);
    let ScenarioNode::Comprehension { comprehension, .. } = node else {
        panic!("expected Comprehension");
    };
    let body = body_of(&comprehension);
    match body {
        Algebra::Union { children } => {
            assert_eq!(children.len(), 2);
            // Each sub-space is a 2-clause Cartesian.
            for child in children {
                match child {
                    Algebra::Cartesian { children: inner } => {
                        assert_eq!(inner.len(), 2);
                        assert_eq!(clause_name(&inner[0]), Some("k"));
                        assert_eq!(clause_name(&inner[1]), Some("limit"));
                    }
                    other => panic!("expected Cartesian sub-space, got {other:?}"),
                }
            }
        }
        other => panic!("expected Union, got {other:?}"),
    }
}

#[test]
fn array_multi_clause_distinct_vars_is_multi_clause_cartesian() {
    let yaml = format!(
        r#"
scenarios:
  s:
    - for_each:
        - "x in 1,2, y in a,b, z in p,q"
      phases: [p]
{PHASES}
"#
    );
    let node = first_scenario_node(&yaml);
    let ScenarioNode::Comprehension { comprehension, .. } = node else {
        panic!("expected Comprehension");
    };
    let body = body_of(&comprehension);
    match body {
        Algebra::Cartesian { children } => {
            assert_eq!(children.len(), 3);
            assert_eq!(clause_name(&children[0]), Some("x"));
            assert_eq!(clause_name(&children[1]), Some("y"));
            assert_eq!(clause_name(&children[2]), Some("z"));
        }
        other => panic!("expected Cartesian, got {other:?}"),
    }
}

#[test]
fn for_keyword_alias_maps_to_same_comprehension() {
    // `for: ...` is accepted as a synonym for `for_each: ...`.
    let yaml = format!(
        r#"
scenarios:
  s:
    - for: "k in 10,100"
      phases: [p]
{PHASES}
"#
    );
    let node = first_scenario_node(&yaml);
    let ScenarioNode::Comprehension { comprehension, .. } = node else {
        panic!("expected Comprehension");
    };
    // Single clause; no filter, no order; coordinate_names = ["k"].
    assert!(filter_of(&comprehension).is_none());
    assert!(!matches!(comprehension, Algebra::Order { .. }));
    assert_eq!(comprehension.coordinate_names(), vec!["k"]);
}

#[test]
fn where_clause_attaches_to_comprehension() {
    let yaml = format!(
        r#"
scenarios:
  s:
    - for: "k in 10,100, limit in 10,20,30"
      where: "{{k}} * {{limit}} < 1000"
      phases: [p]
{PHASES}
"#
    );
    let node = first_scenario_node(&yaml);
    let ScenarioNode::Comprehension { comprehension, .. } = node else {
        panic!("expected Comprehension");
    };
    assert_eq!(filter_of(&comprehension), Some("{k} * {limit} < 1000"));
}

#[test]
fn for_each_in_paren_call_does_not_split() {
    // Regression guard: commas inside `matching_profiles('a','b')`
    // must not be treated as top-level clause separators.
    let yaml = format!(
        r#"
scenarios:
  s:
    - for_each: "p in matching_profiles('ds', 'pre')"
      phases: [p]
{PHASES}
"#
    );
    let node = first_scenario_node(&yaml);
    let ScenarioNode::Comprehension { comprehension, .. } = node else {
        panic!("expected Comprehension");
    };
    let body = body_of(&comprehension);
    assert_eq!(clause_name(body), Some("p"));
    match clause_source(body) {
        Some(Source::Generator { expr, .. }) => {
            assert!(expr.contains("matching_profiles"), "got: {expr}");
            assert!(expr.contains(","));
        }
        other => panic!("expected Generator source, got {other:?}"),
    }
}

// ---- Layer 7a parallel-iter through the YAML parser --------

#[test]
fn parallel_iter_string_form_round_trips_through_yaml() {
    // `(a, b) in (e1, e2)` survives the YAML → algebra path
    // as a top-level Zip with two Clause children and
    // Strict mode.
    let yaml = format!(
        r#"
scenarios:
  s:
    - for_each: "(x, y) in (fib(5), pow2(5))"
      phases: [p]
{PHASES}
"#
    );
    let node = first_scenario_node(&yaml);
    let ScenarioNode::Comprehension { comprehension, .. } = node else {
        panic!("expected Comprehension");
    };
    let body = body_of(&comprehension);
    match body {
        Algebra::Zip { children, mode } => {
            assert_eq!(*mode, ZipMode::Strict);
            assert_eq!(children.len(), 2);
            assert_eq!(clause_name(&children[0]), Some("x"));
            assert_eq!(clause_name(&children[1]), Some("y"));
        }
        other => panic!("expected Zip, got {other:?}"),
    }
}

#[test]
fn parallel_iter_zip_truncate_round_trips_through_yaml() {
    let yaml = format!(
        r#"
scenarios:
  s:
    - for_each: "(x, y) in zip_truncate(fib(5), pow2(3))"
      phases: [p]
{PHASES}
"#
    );
    let node = first_scenario_node(&yaml);
    let ScenarioNode::Comprehension { comprehension, .. } = node else {
        panic!("expected Comprehension");
    };
    let body = body_of(&comprehension);
    match body {
        Algebra::Zip { mode, .. } => {
            assert_eq!(*mode, ZipMode::Truncate);
        }
        other => panic!("expected Zip, got {other:?}"),
    }
}

#[test]
fn parallel_iter_mixed_with_single_clause_round_trips() {
    // `(x, y) in (...), z in zs` — parallel group is one
    // clause, z is another. Cartesian over both: child 0 is
    // a Zip, child 1 is a Clause.
    let yaml = format!(
        r#"
scenarios:
  s:
    - for_each: "(x, y) in (fib(4), pow2(4)), z in 1..3"
      phases: [p]
{PHASES}
"#
    );
    let node = first_scenario_node(&yaml);
    let ScenarioNode::Comprehension { comprehension, .. } = node else {
        panic!("expected Comprehension");
    };
    let body = body_of(&comprehension);
    match body {
        Algebra::Cartesian { children } => {
            assert_eq!(children.len(), 2);
            assert!(matches!(&children[0], Algebra::Zip { .. }));
            assert_eq!(clause_name(&children[1]), Some("z"));
        }
        other => panic!("expected Cartesian, got {other:?}"),
    }
}
