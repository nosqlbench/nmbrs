// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for SRD-72 workload `extends:` semantics.
//!
//! Each test creates a unique subdirectory under the project's
//! configured TMPDIR (`target/test-tmp` per workspace
//! `.cargo/config.toml`) and writes parent + child YAML files
//! into it. Subdirectory names are scoped per-test via a
//! monotonic counter so concurrent test runs don't collide.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nmbrs_workload::parse::{parse_workload, parse_workload_from_path};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Allocate a fresh subdirectory under the configured TMPDIR for
/// this test case. The directory is deliberately NOT deleted —
/// `cargo test` runs leave their artefacts in `target/test-tmp/`
/// for post-hoc inspection; rebuild with `cargo clean` to reset.
fn fresh_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("nmbrs-extends-{tag}-{pid}-{n}"));
    std::fs::create_dir_all(&dir).expect("create fresh tempdir");
    dir
}

fn write_yaml(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write yaml");
    path
}

// ──────────────────────────────────────────────────────────────
// 1. Single-parent: child adds a new scenario, parent's intact
// ──────────────────────────────────────────────────────────────
#[test]
fn child_adds_new_scenario() {
    let dir = fresh_dir("add_scenario");
    write_yaml(
        &dir,
        "parent.yaml",
        r#"
scenarios:
  base:
    - run
phases:
  run:
    ops:
      hello:
        raw: "SELECT 1"
"#,
    );
    let child = write_yaml(
        &dir,
        "child.yaml",
        r#"
extends: ./parent.yaml
scenarios:
  extra:
    - run
"#,
    );
    let wl = parse_workload_from_path(&child, &HashMap::new()).expect("parse merged");
    assert!(
        wl.scenarios.contains_key("base"),
        "parent scenario preserved"
    );
    assert!(wl.scenarios.contains_key("extra"), "child scenario added");
}

// ──────────────────────────────────────────────────────────────
// 2. Child overrides one phase, others stay intact
// ──────────────────────────────────────────────────────────────
#[test]
fn child_overrides_one_phase() {
    let dir = fresh_dir("override_phase");
    write_yaml(
        &dir,
        "parent.yaml",
        r#"
phases:
  a:
    ops: { x: { raw: "PARENT_A" } }
  b:
    ops: { x: { raw: "PARENT_B" } }
"#,
    );
    let child = write_yaml(
        &dir,
        "child.yaml",
        r#"
extends: ./parent.yaml
phases:
  b:
    ops: { x: { raw: "CHILD_B" } }
"#,
    );
    let wl = parse_workload_from_path(&child, &HashMap::new()).expect("parse merged");
    assert!(wl.phases.contains_key("a"));
    assert!(wl.phases.contains_key("b"));
    let b_op = &wl.phases.get("b").unwrap().ops[0];
    let raw = b_op.op.get("raw").and_then(|v| v.as_str()).unwrap_or("");
    assert_eq!(raw, "CHILD_B", "child phase replaced parent's");
    let a_op = &wl.phases.get("a").unwrap().ops[0];
    let raw_a = a_op.op.get("raw").and_then(|v| v.as_str()).unwrap_or("");
    assert_eq!(raw_a, "PARENT_A", "untouched parent phase preserved");
}

// ──────────────────────────────────────────────────────────────
// 3. `bindings:` concat — parent first, child appended
// ──────────────────────────────────────────────────────────────
#[test]
fn bindings_concat_parent_first() {
    let dir = fresh_dir("bindings_concat");
    write_yaml(
        &dir,
        "parent.yaml",
        r#"
bindings: |
  parent_wire := 42
"#,
    );
    let child = write_yaml(
        &dir,
        "child.yaml",
        r#"
extends: ./parent.yaml
bindings: |
  child_wire := 99
"#,
    );
    let wl = parse_workload_from_path(&child, &HashMap::new()).expect("parse merged");
    let src = bindings_source(&wl);
    let p_idx = src.find("parent_wire").expect("parent binding present");
    let c_idx = src.find("child_wire").expect("child binding present");
    assert!(
        p_idx < c_idx,
        "parent binding appears before child binding\nsrc:\n{src}"
    );
}

// ──────────────────────────────────────────────────────────────
// 4. `status_metrics:` union with dedup, first-occurrence order
// ──────────────────────────────────────────────────────────────
#[test]
fn status_metrics_union_dedup() {
    let dir = fresh_dir("status_union");
    write_yaml(
        &dir,
        "parent.yaml",
        r#"
status_metrics: [a, b, c]
"#,
    );
    let child = write_yaml(
        &dir,
        "child.yaml",
        r#"
extends: ./parent.yaml
status_metrics: [b, d]
"#,
    );
    let wl = parse_workload_from_path(&child, &HashMap::new()).expect("parse merged");
    assert_eq!(wl.status_metrics, vec!["a", "b", "c", "d"]);
}

// ──────────────────────────────────────────────────────────────
// 5. `report:` per-section merge
// ──────────────────────────────────────────────────────────────
#[test]
fn report_per_section_merge() {
    let dir = fresh_dir("report_merge");
    write_yaml(
        &dir,
        "parent.yaml",
        r#"
report:
  oracles_section: |
    file oracles_report.md as 'Oracles'
phases:
  q:
    ops: { x: { raw: "Q" } }
"#,
    );
    let child = write_yaml(
        &dir,
        "child.yaml",
        r#"
extends: ./parent.yaml
report:
  sweep_section: |
    file sweep_report.md as 'Sweep'
"#,
    );
    let wl = parse_workload_from_path(&child, &HashMap::new()).expect("parse merged");
    let group_names: Vec<&str> = wl.report.groups.iter().map(|g| g.name.as_str()).collect();
    assert!(
        group_names.contains(&"oracles_section"),
        "parent section preserved: {group_names:?}"
    );
    assert!(
        group_names.contains(&"sweep_section"),
        "child section added: {group_names:?}"
    );
}

// ──────────────────────────────────────────────────────────────
// 6. `params:` three-layer precedence: caller > child > parent
// ──────────────────────────────────────────────────────────────
#[test]
fn params_three_layer_precedence() {
    let dir = fresh_dir("params_layered");
    write_yaml(
        &dir,
        "parent.yaml",
        r#"
params:
  a: parent_a
  b: parent_b
  c: parent_c
"#,
    );
    let child = write_yaml(
        &dir,
        "child.yaml",
        r#"
extends: ./parent.yaml
params:
  b: child_b
  c: child_c
"#,
    );
    let mut caller = HashMap::new();
    caller.insert("c".to_string(), "caller_c".to_string());
    let wl = parse_workload_from_path(&child, &caller).expect("parse merged");
    assert_eq!(wl.params.get("a").map(String::as_str), Some("parent_a"));
    assert_eq!(wl.params.get("b").map(String::as_str), Some("child_b"));
    assert_eq!(wl.params.get("c").map(String::as_str), Some("caller_c"));
}

// ──────────────────────────────────────────────────────────────
// 7. Two-level chain: grandparent → parent → child
// ──────────────────────────────────────────────────────────────
#[test]
fn two_level_chain() {
    let dir = fresh_dir("two_level");
    write_yaml(
        &dir,
        "grandparent.yaml",
        r#"
params: { from_gp: 1 }
phases:
  gp_phase:
    ops: { x: { raw: "GP" } }
"#,
    );
    write_yaml(
        &dir,
        "parent.yaml",
        r#"
extends: ./grandparent.yaml
params: { from_p: 2 }
phases:
  p_phase:
    ops: { x: { raw: "P" } }
"#,
    );
    let child = write_yaml(
        &dir,
        "child.yaml",
        r#"
extends: ./parent.yaml
params: { from_c: 3 }
phases:
  c_phase:
    ops: { x: { raw: "C" } }
"#,
    );
    let wl = parse_workload_from_path(&child, &HashMap::new()).expect("parse merged");
    assert!(
        wl.phases.contains_key("gp_phase"),
        "grandparent phase present"
    );
    assert!(wl.phases.contains_key("p_phase"), "parent phase present");
    assert!(wl.phases.contains_key("c_phase"), "child phase present");
    assert_eq!(wl.params.get("from_gp").map(String::as_str), Some("1"));
    assert_eq!(wl.params.get("from_p").map(String::as_str), Some("2"));
    assert_eq!(wl.params.get("from_c").map(String::as_str), Some("3"));
}

// ──────────────────────────────────────────────────────────────
// 8. Cycle detection: a → b → a
// ──────────────────────────────────────────────────────────────
#[test]
fn cycle_detection() {
    let dir = fresh_dir("cycle");
    let a = write_yaml(
        &dir,
        "a.yaml",
        r#"
extends: ./b.yaml
"#,
    );
    write_yaml(
        &dir,
        "b.yaml",
        r#"
extends: ./a.yaml
"#,
    );
    let err = parse_workload_from_path(&a, &HashMap::new()).expect_err("cycle must be rejected");
    assert!(err.contains("cycle"), "error mentions cycle: {err}");
    assert!(err.contains("a.yaml"), "error names a.yaml: {err}");
    assert!(err.contains("b.yaml"), "error names b.yaml: {err}");
}

// ──────────────────────────────────────────────────────────────
// 9. Missing-file error includes resolved absolute path
// ──────────────────────────────────────────────────────────────
#[test]
fn missing_parent_file_error() {
    let dir = fresh_dir("missing_parent");
    let child = write_yaml(
        &dir,
        "child.yaml",
        r#"
extends: ./does_not_exist.yaml
"#,
    );
    let err =
        parse_workload_from_path(&child, &HashMap::new()).expect_err("missing parent must error");
    assert!(
        err.contains("does_not_exist.yaml"),
        "error names the missing path: {err}"
    );
}

// ──────────────────────────────────────────────────────────────
// 10. Path resolution relative to including file (nested dirs)
// ──────────────────────────────────────────────────────────────
#[test]
fn relative_path_from_including_file() {
    let dir = fresh_dir("rel_path");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    write_yaml(
        &dir,
        "parent.yaml",
        r#"
phases:
  p:
    ops: { x: { raw: "P" } }
"#,
    );
    let child = write_yaml(
        &dir.join("sub"),
        "child.yaml",
        r#"
extends: ../parent.yaml
phases:
  c:
    ops: { x: { raw: "C" } }
"#,
    );
    let wl = parse_workload_from_path(&child, &HashMap::new()).expect("parse merged");
    assert!(wl.phases.contains_key("p"));
    assert!(wl.phases.contains_key("c"));
}

// ──────────────────────────────────────────────────────────────
// 11. Parent YAML error: child path mentioned in wrapped error
// ──────────────────────────────────────────────────────────────
#[test]
fn parent_yaml_error_wrapped() {
    let dir = fresh_dir("parent_err");
    // Tab in indentation + dangling colon-only key = serde_yaml
    // parse error (YAML 1.2 prohibits tabs for indentation).
    write_yaml(&dir, "parent.yaml", "phases:\n\t:\n  - bogus\n");
    let child = write_yaml(
        &dir,
        "child.yaml",
        r#"
extends: ./parent.yaml
"#,
    );
    let err =
        parse_workload_from_path(&child, &HashMap::new()).expect_err("parent error must propagate");
    assert!(
        err.contains("parent.yaml"),
        "error mentions parent path: {err}"
    );
}

// ──────────────────────────────────────────────────────────────
// 12. `extends:` as list value → parse error
// ──────────────────────────────────────────────────────────────
#[test]
fn extends_list_value_rejected() {
    let dir = fresh_dir("ext_list");
    let child = write_yaml(
        &dir,
        "child.yaml",
        r#"
extends: [./a.yaml, ./b.yaml]
"#,
    );
    let err =
        parse_workload_from_path(&child, &HashMap::new()).expect_err("list extends must error");
    assert!(
        err.contains("scalar string") || err.contains("list"),
        "error explains type mismatch: {err}"
    );
}

// ──────────────────────────────────────────────────────────────
// 13. `extends:` nested inside a phase → not honoured (the
//     extends-loader only looks at the top-level mapping;
//     nested 'extends' is just a regular yaml key with no
//     special meaning to this layer).
// ──────────────────────────────────────────────────────────────
#[test]
fn nested_extends_inert() {
    let dir = fresh_dir("ext_nested");
    let child = write_yaml(
        &dir,
        "child.yaml",
        r#"
phases:
  p:
    extends: ./not_a_real_parent.yaml
    ops: { x: { raw: "X" } }
"#,
    );
    // No file load was attempted (otherwise we'd get a not-found
    // error). The workload itself may not normalise cleanly
    // because of the unknown 'extends' key inside a phase — we
    // only assert no extends-resolution path was triggered.
    let result = parse_workload_from_path(&child, &HashMap::new());
    match result {
        Ok(_) => { /* phase parser ignored the nested key */ }
        Err(e) => assert!(
            !e.contains("not_a_real_parent.yaml") && !e.contains("cycle"),
            "no extends-loader path triggered for nested key: {e}"
        ),
    }
}

// ──────────────────────────────────────────────────────────────
// 14. Parent partial (no scenarios) + complete child → ok
// ──────────────────────────────────────────────────────────────
#[test]
fn partial_parent_complete_child() {
    let dir = fresh_dir("partial_parent");
    write_yaml(
        &dir,
        "parent.yaml",
        r#"
params:
  shared: yes
"#,
    );
    let child = write_yaml(
        &dir,
        "child.yaml",
        r#"
extends: ./parent.yaml
phases:
  p:
    ops: { x: { raw: "X" } }
scenarios:
  default:
    - p
"#,
    );
    let wl = parse_workload_from_path(&child, &HashMap::new()).expect("merged workload validates");
    assert!(wl.scenarios.contains_key("default"));
    assert_eq!(wl.params.get("shared").map(String::as_str), Some("yes"));
}

// ──────────────────────────────────────────────────────────────
// 15. Top-level `ops:` map-form per-name override
// ──────────────────────────────────────────────────────────────
#[test]
fn top_ops_map_form_per_name_override() {
    let dir = fresh_dir("ops_map");
    write_yaml(
        &dir,
        "parent.yaml",
        r#"
ops:
  a: { raw: "PARENT_A" }
  b: { raw: "PARENT_B" }
"#,
    );
    let child = write_yaml(
        &dir,
        "child.yaml",
        r#"
extends: ./parent.yaml
ops:
  b: { raw: "CHILD_B" }
  c: { raw: "CHILD_C" }
"#,
    );
    let wl = parse_workload_from_path(&child, &HashMap::new()).expect("parse merged");
    let names: Vec<&str> = wl.ops.iter().map(|o| o.name.as_str()).collect();
    assert!(names.contains(&"a"), "parent op a preserved: {names:?}");
    assert!(names.contains(&"b"), "op b present: {names:?}");
    assert!(names.contains(&"c"), "child op c added: {names:?}");
    let b = wl.ops.iter().find(|o| o.name == "b").unwrap();
    let raw = b.op.get("raw").and_then(|v| v.as_str()).unwrap_or("");
    assert_eq!(raw, "CHILD_B", "child override applied to op b");
}

// ──────────────────────────────────────────────────────────────
// 16. Top-level `ops:` list-form whole-replace
// ──────────────────────────────────────────────────────────────
#[test]
fn top_ops_list_form_whole_replace() {
    let dir = fresh_dir("ops_list");
    write_yaml(
        &dir,
        "parent.yaml",
        r#"
ops:
  - "PARENT_OP_1"
  - "PARENT_OP_2"
"#,
    );
    let child = write_yaml(
        &dir,
        "child.yaml",
        r#"
extends: ./parent.yaml
ops:
  - "CHILD_OP_1"
"#,
    );
    let wl = parse_workload_from_path(&child, &HashMap::new()).expect("parse merged");
    // The list form is position-keyed; whole-replace means only
    // the child's op survives. Parent's positional ops are gone.
    let raws: Vec<&str> = wl
        .ops
        .iter()
        .filter_map(|o| o.op.get("stmt").and_then(|v| v.as_str()))
        .collect();
    assert!(
        raws.iter().any(|r| r.contains("CHILD_OP_1")),
        "child op present: {raws:?}"
    );
    assert!(
        !raws.iter().any(|r| r.contains("PARENT_OP")),
        "parent positional ops replaced: {raws:?}"
    );
}

// ──────────────────────────────────────────────────────────────
// 17. Text-only `parse_workload(&str, _)` rejects `extends:`
//     because there's no resolution context.
// ──────────────────────────────────────────────────────────────
#[test]
fn in_memory_parser_rejects_extends() {
    let yaml = r#"
extends: ./somewhere.yaml
phases:
  p:
    ops: { x: { raw: "X" } }
"#;
    let err =
        parse_workload(yaml, &HashMap::new()).expect_err("text-only entry must reject extends");
    assert!(
        err.contains("extends") && err.contains("parse_workload_from_path"),
        "error directs user to the path-based entry: {err}"
    );
}

// ──────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────

/// Return the Polydat source string from a Workload's `bindings`
/// field, regardless of whether it was string-form or map-form
/// in the YAML.
fn bindings_source(wl: &nmbrs_workload::model::Workload) -> String {
    match &wl.bindings {
        nmbrs_workload::model::BindingsDef::PolydatSource(s) => s.clone(),
        nmbrs_workload::model::BindingsDef::Map(entries) => entries
            .iter()
            .map(|(k, v)| format!("{k} := {v}"))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}
