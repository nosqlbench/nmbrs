// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-71 probe: verify the scope tree structure produced from
//! the production YAML matches what the comprehension iteration
//! over `partitions(...)` needs.

use nmbrs_runtime::scope_tree::{ScopeKind, ScopeTree};
use nmbrs_workload::parse::parse_workload;
use std::collections::HashMap;

#[test]
fn for_each_partition_workload_produces_comprehension_scope() {
    let yaml = r#"
scenarios:
  sweep:
    - for: "p in partitions(\"linear:3\")"
      phases:
        - walk

phases:
  walk:
    cycles: 2
    concurrency: 1
    bindings: |
      n := mod_in(cycle, p)
    ops:
      emit:
        adapter: stdout
        stmt: "n={n}"
"#;
    let workload = parse_workload(yaml, &HashMap::new()).expect("YAML should parse");

    // Resolve scenario name "sweep" to its tree of scope nodes.
    let scenario_nodes: Vec<_> = workload
        .scenarios
        .get("sweep")
        .expect("scenario `sweep` should exist")
        .clone();

    let tree = ScopeTree::build("sweep", &scenario_nodes);

    // Print the tree structure for diagnostic purposes.
    let kinds: Vec<String> = tree
        .iter_dfs()
        .map(|(idx, n)| format!("[{idx}] {}", n.kind.label()))
        .collect();
    eprintln!("scope tree:\n{}", kinds.join("\n"));

    // The Comprehension scope must exist somewhere in the tree
    // — otherwise the iteration over `partitions(...)` is lost.
    let comprehension_count = tree
        .iter_dfs()
        .filter(|(_, n)| matches!(n.kind, ScopeKind::Comprehension { .. }))
        .count();
    assert_eq!(
        comprehension_count,
        1,
        "expected exactly one Comprehension scope; got {comprehension_count}.\n{}",
        kinds.join("\n")
    );

    // Inspect the parsed Comprehension scope's clause and assert
    // that the spec text round-trips intact. A quote-stripping
    // bug in the YAML / clause parser would surface here as
    // `partitions(linear:3)` (no quotes), which would then fail
    // to evaluate as a const partition list.
    let (_, comp_node) = tree
        .iter_dfs()
        .find(|(_, n)| matches!(n.kind, ScopeKind::Comprehension { .. }))
        .expect("comp scope present");
    if let ScopeKind::Comprehension { comprehension } = &comp_node.kind {
        let pairs = comprehension.coordinate_specs();
        assert_eq!(pairs.len(), 1, "single-clause for-each");
        let (var, spec) = &pairs[0];
        assert_eq!(var, "p", "var name");
        assert_eq!(
            spec, "partitions(\"linear:3\")",
            "spec text must round-trip with quotes intact, got {spec:?}"
        );
    }
}

/// Simulate the runner's install loop:
/// 1. Workload-root kernel.
/// 2. Comprehension scope → `synthesize_for_each_scope`.
/// 3. Phase walk → `build_phase_scope_kernel` with comp scope as parent.
///
/// Asserts that after the cascade, the phase kernel sees `p`
/// as `PortType::Ext`. Reproduces the production install chain
/// without the YAML / scenario / pre-map machinery, so any
/// discrepancy with my finer-grained unit tests surfaces here.
#[test]
fn install_chain_preserves_partition_iter_var_type_through_phase() {
    use nmbrs_runtime::scope::build_phase_scope_kernel;
    use nmbrs_workload::model::BindingsDef;
    use polydat::ast::PortType;
    use std::sync::Arc;

    // Workload-root: matches what build_workload_params_kernel
    // produces for an empty `params:` block — a kernel with no
    // outputs of its own. Tests at higher layers verified that
    // pre-eval correctly classifies `partitions(...)` as a
    // PartitionList; this test exercises the post-pre-eval
    // synthesis chain.
    let workload_root = polydat::dsl::compile_polydat_interpreter("\n").unwrap();

    // Comprehension scope: install as the runner does.
    let comp_kernel = nmbrs_runtime::scope_synth::build_for_each_scope_kernel(
        &[("p".to_string(), "partitions(\"linear:3\")".to_string())],
        &[], // empty parent_manifest
        &workload_root,
        &HashMap::new(),
        Vec::new(),
        None,
        false,
        "comp_install",
        None,
    )
    .expect("comp scope install");

    // Sanity: comp kernel has `p` as Ext-typed.
    assert_eq!(
        comp_kernel.program().input_port_type("p"),
        Some(PortType::Ext),
        "comp scope must declare `p` as Ext",
    );

    // Phase walk: build with comp_kernel as parent (matches the
    // runner's nearest_installed_ancestor lookup).
    let _arc = Arc::new(comp_kernel);
    let comp_kernel = _arc;
    let phase_bindings = BindingsDef::PolydatSource("n := mod_in(cycle, p)\n".to_string());
    let phase_kernel = build_phase_scope_kernel(
        &phase_bindings,
        &[], // parent_manifest (extract_manifest would populate this in the runner)
        &comp_kernel,
        &HashMap::new(),
        Vec::new(),
        None,
        false,
        "phase_install",
    )
    .expect("phase kernel build");

    // Critical assertion: `p` must be Ext-typed at the phase
    // kernel — anything else (u64, Str) is the production
    // failure mode.
    assert_eq!(
        phase_kernel.program().input_port_type("p"),
        Some(PortType::Ext),
        "phase kernel must declare `p` as Ext via the cascade",
    );
}

/// Closest-to-production reproduction: runs the actual runner
/// pipeline by parsing a real workload, building the scope tree,
/// and walking the install_specs. Asserts the phase kernel ends
/// up with `p` typed as Ext.
///
/// This catches discrepancies between my finer-grained unit
/// tests and the runner's actual sequencing (install order,
/// parent-kernel resolution, etc.).
#[test]
fn end_to_end_install_through_runner_pipeline_yields_ext_p() {
    use nmbrs_runtime::scope_tree::{ScopeKind, ScopeTree};
    use nmbrs_workload::parse::parse_workload;
    use polydat::ast::PortType;
    use std::sync::Arc;

    let yaml = r#"
scenarios:
  sweep:
    - for: "p in partitions(\"linear:3\")"
      phases:
        - walk

phases:
  walk:
    cycles: 2
    concurrency: 1
    bindings: |
      n := mod_in(cycle, p)
    ops:
      emit:
        adapter: stdout
        stmt: "n={n}"
"#;
    let workload = parse_workload(yaml, &HashMap::new()).expect("parse_workload");

    let scenario_nodes = workload
        .scenarios
        .get("sweep")
        .expect("sweep scenario")
        .clone();
    let mut scope_tree = ScopeTree::build("sweep", &scenario_nodes);
    // Mirror the runner: extend with op-template children before
    // installing kernels.
    scope_tree.extend_with_op_templates(&workload.phases);

    // Workload-root install: minimal kernel (empty params).
    let workload_root =
        Arc::new(polydat::dsl::compile_polydat_interpreter("\n").expect("workload root compile"));
    scope_tree.install_kernel(scope_tree.root, workload_root.clone());

    // Walk DFS, install Comprehension and Phase kernels in
    // order — mirrors the runner's install_specs loop.
    for (idx, node) in scope_tree.iter_dfs() {
        match &node.kind {
            ScopeKind::Comprehension { comprehension } => {
                let bindings = comprehension.coordinate_specs();

                // Nearest installed ancestor:
                let mut cursor = node.parent;
                let parent_kernel = loop {
                    let Some(p) = cursor else {
                        panic!("no installed ancestor for comprehension");
                    };
                    if let Some(k) = scope_tree.nodes[p].cached_kernel.get() {
                        break k.clone();
                    }
                    cursor = scope_tree.nodes[p].parent;
                };

                let comp_kernel = nmbrs_runtime::scope_synth::build_for_each_scope_kernel(
                    &bindings,
                    &[],
                    &parent_kernel,
                    &HashMap::new(),
                    Vec::new(),
                    None,
                    false,
                    &format!("scope idx {idx}"),
                    None,
                )
                .expect("comp scope kernel");
                scope_tree.install_kernel(idx, Arc::new(comp_kernel));
            }
            ScopeKind::Phase { name } => {
                if name != "walk" {
                    continue;
                }
                let phase = workload
                    .phases
                    .get(name.as_str())
                    .expect("phase walk in workload");

                let mut cursor = node.parent;
                let parent_kernel = loop {
                    let Some(p) = cursor else {
                        panic!("no installed ancestor for phase");
                    };
                    if let Some(k) = scope_tree.nodes[p].cached_kernel.get() {
                        break k.clone();
                    }
                    cursor = scope_tree.nodes[p].parent;
                };

                let phase_kernel = nmbrs_runtime::scope::build_phase_scope_kernel(
                    &phase.bindings,
                    &[],
                    &parent_kernel,
                    &HashMap::new(),
                    Vec::new(),
                    None,
                    false,
                    &format!("scope idx {idx} (phase '{name}')"),
                )
                .expect("phase kernel build");

                assert_eq!(
                    phase_kernel.program().input_port_type("p"),
                    Some(PortType::Ext),
                    "phase kernel via runner pipeline must declare `p` as Ext",
                );
                return;
            }
            _ => {}
        }
    }
    panic!("phase `walk` not reached — install loop didn't find it");
}
