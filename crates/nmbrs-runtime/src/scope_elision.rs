// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-13d Phase 3 — workload-init scope-elision pre-walk.
//!
//! Pulls together the AST-side classification
//! ([`nmbrs_workload::polydat_matter::HasPolydatMatter`]) and the scope-
//! tree marking ([`crate::scope_tree::ScopeTree::mark_scope_elision`])
//! to produce a fully-marked scope tree before any kernel
//! instances exist.
//!
//! The pre-walk runs **once per workload load**, between the
//! scope-tree build and the kernel installations. After it
//! finishes:
//!
//! - Every node has `materialised: Some(true|false)`.
//! - Every node has a non-empty `logical_name` per SRD-13d §5.3.
//! - Premap and runtime can call
//!   [`crate::scope_tree::ScopeTree::nearest_materialised`]
//!   to walk past elided tiers safely.
//!
//! Today's predicate is **conservative**: any AST node that
//! classifies as `PolydatMatter::Definitions` materialises. The
//! hash-subset refinement (SRD-13d §3.2 step 3.ii — "the
//! `Definitions` content collapses by hash") is reserved for
//! Phase 6 (premap descent + per-op-template kernel
//! compilation), since it requires program objects that don't
//! yet exist at workload-load time.
//!
//! Even with the conservative predicate, the cheap path
//! (`None` / `Readonly` → elide) covers the bulk of real
//! workloads — most op templates have no Polydat content beyond
//! parent-scope reads.

use std::collections::HashMap;

use nmbrs_workload::model::{BindingsDef, ParsedOp, WorkloadPhase};
use nmbrs_workload::polydat_matter::{HasPolydatMatter, PolydatMatter};

use crate::scope_tree::{ScopeKind, ScopeNodeIdx, ScopeTree};

/// Inputs the pre-walk consults. Decoupled from the full
/// `Workload` struct so the call site can supply borrowed
/// references even when other fields (e.g. `workload.ops`)
/// have already been partially moved into local mut-bindings
/// by the runner.
pub struct ClassifyInputs<'a> {
    /// Workload-level `bindings:` block (top-level YAML).
    pub bindings: &'a BindingsDef,
    /// Workload-level params. A non-empty map promotes the
    /// workload root to `Definitions` (each param becomes a
    /// `final <name> := <literal>` binding on the workload-
    /// params kernel; SRD-13d §3.1).
    pub params: &'a HashMap<String, String>,
    /// Per-phase AST nodes keyed by phase name.
    pub phases: &'a HashMap<String, WorkloadPhase>,
}

/// Run the SRD-13d Phase 3 scope-elision pre-walk on a
/// freshly-built scope tree. Reads the workload AST to
/// classify each scope-tree node; calls
/// [`ScopeTree::mark_scope_elision`] with the resulting
/// predicate.
///
/// Conservative today (Definitions ⇒ materialise without
/// hash-subset refinement). Phase 6 will tighten the
/// predicate by adding the program-hash check; the call site
/// stays the same.
pub fn classify_and_mark(tree: &mut ScopeTree, inputs: &ClassifyInputs<'_>) {
    // Pre-compute the owning-phase name for every OpTemplate node
    // by walking up to the nearest Phase ancestor. Op names are
    // not globally unique — two phases can each declare an op
    // called `select_ann` with very different bodies — so the
    // flat `phases.values().flat_map(|p| p.ops)` lookup that used
    // to live in `scope_kind_polydat_matter` could silently pick the
    // wrong phase's op and apply the wrong classification.
    // Mirrors the same disambiguation `runner.rs::InstallSpec::OpTemplate`
    // already does at install time.
    let mut owning_phase: std::collections::HashMap<ScopeNodeIdx, String> =
        std::collections::HashMap::new();
    for (idx, node) in tree.iter_dfs() {
        if !matches!(node.kind, ScopeKind::OpTemplate { .. }) {
            continue;
        }
        let mut cursor = node.parent;
        while let Some(p) = cursor {
            if let ScopeKind::Phase { name } = &tree.nodes[p].kind {
                owning_phase.insert(idx, name.clone());
                break;
            }
            cursor = tree.nodes[p].parent;
        }
    }

    tree.mark_scope_elision(|kind, idx| {
        let matter = scope_kind_polydat_matter(kind, idx, inputs, &owning_phase);
        matches!(matter, PolydatMatter::Definitions)
    });
}

/// Map a scope-tree `ScopeKind` to the AST node's
/// `PolydatMatter` classification.
///
/// - **Workload root** — consults the top-level `bindings:`
///   block and the workload-params map.
/// - **Scenario** — `None`. Scenario nodes don't carry GK
///   content of their own; the underlying `ScenarioNode`
///   children do.
/// - **Phase** — looks up the named phase and consults
///   `WorkloadPhase::polydat_matter` (phase-level `bindings:`,
///   `for_each:`, `cycles=` parent refs).
/// - **Comprehension / DoWhile / DoUntil** — Always
///   `Definitions`: iteration constructs bind iteration
///   variables by definition.
/// - **IncludedScenario** — `None`. The wrapper itself adds
///   nothing; the included scenario's children carry the
///   classification.
fn scope_kind_polydat_matter(
    kind: &ScopeKind,
    idx: ScopeNodeIdx,
    inputs: &ClassifyInputs<'_>,
    owning_phase: &std::collections::HashMap<ScopeNodeIdx, String>,
) -> PolydatMatter {
    match kind {
        // SRD-88 — the session root owns the session polydat scope
        // (process/session args); no per-execution matter of its own.
        ScopeKind::Session => PolydatMatter::None,
        ScopeKind::Workload => {
            // Mirrors `Workload::polydat_matter` without requiring
            // the whole struct.
            if !inputs.bindings.is_empty() || !inputs.params.is_empty() {
                PolydatMatter::Definitions
            } else {
                PolydatMatter::None
            }
        }
        ScopeKind::Scenario { .. } => PolydatMatter::None,
        ScopeKind::Phase { name } => inputs
            .phases
            .get(name)
            .map(WorkloadPhase::polydat_matter)
            .unwrap_or(PolydatMatter::None),
        ScopeKind::OpTemplate { name } => {
            // SRD-13d §3.1 OpTemplate classification: look up the
            // op against its OWNING phase (resolved via the
            // pre-computed ancestor walk). A flat name-only lookup
            // would silently pick the wrong phase's op when two
            // phases declare ops with the same name.
            let phase_name = match owning_phase.get(&idx) {
                Some(n) => n,
                None => return PolydatMatter::None,
            };
            inputs
                .phases
                .get(phase_name)
                .and_then(|p| p.ops.iter().find(|op| op.name == *name))
                .map(ParsedOp::polydat_matter)
                .unwrap_or(PolydatMatter::None)
        }
        ScopeKind::Comprehension { .. } | ScopeKind::DoWhile { .. } | ScopeKind::DoUntil { .. } => {
            PolydatMatter::Definitions
        }
        ScopeKind::IncludedScenario { .. } => PolydatMatter::None,
        // Scenario-tree-level `bindings:` (and the canonical
        // lowered form of `set:`) install Polydat matter — same
        // category as the iteration constructs.
        ScopeKind::Bindings { .. } => PolydatMatter::Definitions,
    }
}

/// Diagnostic helper: enumerate every scope node's mark and
/// logical name. Used by `dryrun=op` and `nmbrs describe wiring`
/// (when SRD-13d phases 7 / 8 fully wire those surfaces).
/// Returns `(idx, depth, materialised, logical_name,
/// kind_label)` quintuples in DFS order.
pub fn elision_summary(
    tree: &ScopeTree,
) -> Vec<(ScopeNodeIdx, usize, Option<bool>, String, String)> {
    tree.iter_dfs()
        .map(|(idx, node)| {
            (
                idx,
                node.depth,
                node.materialised,
                node.logical_name.clone(),
                node.kind.label().to_string(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmbrs_workload::model::{BindingsDef, ScenarioNode, WorkloadPhase};

    fn empty_phase() -> WorkloadPhase {
        WorkloadPhase {
            key_metrics: Vec::new(),
            dimensions: Default::default(),
            cycles: None,
            concurrency: None,
            rate: None,
            daemon: false,
            adapter: None,
            errors: None,
            tries: None,
            tries_backoff: None,
            interval: None,
            repeat: None,
            error_rate_max: None,
            timeout: None,
            stop_when: Vec::new(),
            throttle: None,
            continue_if: None,
            tags: None,
            ops: vec![],
            for_each: None,
            loop_scope: None,
            iter_scope: None,
            checkpoint: None,
            status_metrics: vec![],
            metrics: Default::default(),
            bindings: BindingsDef::default(),
            poll: None,
            optimize: None,
        }
    }

    /// Test helper: build a `ClassifyInputs` from owned data
    /// and run `classify_and_mark` with it.
    fn mark_with(
        tree: &mut ScopeTree,
        bindings: &BindingsDef,
        params: &HashMap<String, String>,
        phases: &HashMap<String, WorkloadPhase>,
    ) {
        let inputs = ClassifyInputs {
            bindings,
            params,
            phases,
        };
        classify_and_mark(tree, &inputs);
    }

    #[test]
    fn empty_workload_elides_everything_below_root() {
        let mut phases = HashMap::new();
        phases.insert("p".into(), empty_phase());
        let mut tree = ScopeTree::build("default", &[ScenarioNode::Phase("p".into())]);
        mark_with(&mut tree, &BindingsDef::default(), &HashMap::new(), &phases);
        // Root materialises (always, per SRD-13d §5.1).
        assert_eq!(tree.nodes[0].materialised, Some(true));
        let scenario_idx = tree.nodes[tree.nodes[0].children[0]].children[0];
        let phase_idx = tree.nodes[scenario_idx].children[0];
        assert_eq!(tree.nodes[scenario_idx].materialised, Some(false));
        assert_eq!(tree.nodes[phase_idx].materialised, Some(false));
        assert_eq!(tree.nodes[0].logical_name, "");
        assert_eq!(
            tree.nodes[scenario_idx].logical_name,
            "workload.scenario.default"
        );
        assert_eq!(
            tree.nodes[phase_idx].logical_name,
            "workload.scenario.default.phase.p"
        );
    }

    #[test]
    fn phase_with_bindings_materialises() {
        let mut phases = HashMap::new();
        let mut p1 = empty_phase();
        p1.bindings = BindingsDef::PolydatSource("k := 5".into());
        phases.insert("p1".into(), p1);
        phases.insert("p2".into(), empty_phase());
        let mut tree = ScopeTree::build(
            "default",
            &[
                ScenarioNode::Phase("p1".into()),
                ScenarioNode::Phase("p2".into()),
            ],
        );
        mark_with(&mut tree, &BindingsDef::default(), &HashMap::new(), &phases);
        let scenario_idx = tree.nodes[tree.nodes[0].children[0]].children[0];
        let p1_idx = tree.nodes[scenario_idx].children[0];
        let p2_idx = tree.nodes[scenario_idx].children[1];
        assert_eq!(tree.nodes[p1_idx].materialised, Some(true));
        assert_eq!(tree.nodes[p2_idx].materialised, Some(false));
    }

    #[test]
    fn workload_with_top_level_bindings_materialises_root() {
        let mut phases = HashMap::new();
        phases.insert("p".into(), empty_phase());
        let bindings = BindingsDef::PolydatSource("dataset := \"sift\"".into());
        let mut tree = ScopeTree::build("default", &[ScenarioNode::Phase("p".into())]);
        mark_with(&mut tree, &bindings, &HashMap::new(), &phases);
        // Workload root predicate returns Definitions due to
        // the bindings — the root is materialised either way
        // by SRD-13d §5.1, but we exercise the predicate path.
        assert_eq!(tree.nodes[0].materialised, Some(true));
    }

    #[test]
    fn workload_with_params_classifies_root_as_definitions() {
        // Non-empty params alone makes the workload root
        // contribute Definitions (each becomes a `final
        // <name> := <literal>` on the workload-params kernel).
        let mut phases = HashMap::new();
        phases.insert("p".into(), empty_phase());
        let mut params = HashMap::new();
        params.insert("dataset".into(), "sift".into());
        let mut tree = ScopeTree::build("default", &[ScenarioNode::Phase("p".into())]);
        mark_with(&mut tree, &BindingsDef::default(), &params, &phases);
        assert_eq!(tree.nodes[0].materialised, Some(true));
    }

    #[test]
    fn comprehension_node_always_materialises() {
        let mut phases = HashMap::new();
        phases.insert("p".into(), empty_phase());
        // Use the cartesian helper so this test stays
        // resilient to changes in the Comprehension struct's
        // private fields.
        let comp = polydat::iteration::comprehension::Comprehension::cartesian(vec![]);
        let mut tree = ScopeTree::build(
            "default",
            &[ScenarioNode::Comprehension {
                anchor: None,
                comprehension: comp,
                children: vec![ScenarioNode::Phase("p".into())],
                continue_if: None,
            }],
        );
        mark_with(&mut tree, &BindingsDef::default(), &HashMap::new(), &phases);
        let scenario_idx = tree.nodes[tree.nodes[0].children[0]].children[0];
        let comp_idx = tree.nodes[scenario_idx].children[0];
        assert_eq!(tree.nodes[comp_idx].materialised, Some(true));
    }

    #[test]
    fn op_template_with_metrics_materialises() {
        // SRD-13d Phase 6 + 40b — an op declaring `metrics:`
        // with a non-bare-name value contributes Definitions
        // and materialises. Bare-name `value:` references
        // resolve to parent bindings (Readonly) and elide.
        use nmbrs_workload::model::{MetricSpec, ParsedOp};
        let mut phases = HashMap::new();
        let mut p = empty_phase();
        let mut op = ParsedOp::simple("a", "noop");
        op.metrics.insert(
            "m".into(),
            MetricSpec {
                cell: Default::default(),
                value: "factor * 2.0".into(), // expression → Definitions
                family: None,
                kind: None,
                unit: None,
                format: None,
            },
        );
        p.ops.push(op);
        phases.insert("p".into(), p);
        let mut tree = ScopeTree::build("default", &[ScenarioNode::Phase("p".into())]);
        // Build the op tier first so the predicate sees it.
        tree.extend_with_op_templates(&phases);
        mark_with(&mut tree, &BindingsDef::default(), &HashMap::new(), &phases);
        // Find the op-template node.
        let op_idx = tree
            .iter_dfs()
            .find(|(_, n)| {
                matches!(&n.kind,
                crate::scope_tree::ScopeKind::OpTemplate { name } if name == "a")
            })
            .map(|(i, _)| i)
            .expect("op-template node");
        assert_eq!(tree.nodes[op_idx].materialised, Some(true));
    }

    #[test]
    fn op_template_bare_name_metric_materialises() {
        // Post-SRD-68 follow-up: every metric, including a
        // bare-name `value:`, requires the op-template kernel
        // because the synthesiser appends a
        // `__metric_<name> := <value>` binding to the kernel's
        // result-binding source. The op MUST materialise so the
        // synthesised binding has somewhere to land.
        use nmbrs_workload::model::{MetricSpec, ParsedOp};
        let mut phases = HashMap::new();
        let mut p = empty_phase();
        let mut op = ParsedOp::simple("a", "noop");
        op.metrics.insert(
            "m".into(),
            MetricSpec {
                cell: Default::default(),
                value: "existing_wire".into(),
                family: None,
                kind: None,
                unit: None,
                format: None,
            },
        );
        p.ops.push(op);
        phases.insert("p".into(), p);
        let mut tree = ScopeTree::build("default", &[ScenarioNode::Phase("p".into())]);
        tree.extend_with_op_templates(&phases);
        mark_with(&mut tree, &BindingsDef::default(), &HashMap::new(), &phases);
        let op_idx = tree
            .iter_dfs()
            .find(|(_, n)| {
                matches!(&n.kind,
                crate::scope_tree::ScopeKind::OpTemplate { name } if name == "a")
            })
            .map(|(i, _)| i)
            .expect("op-template node");
        assert_eq!(tree.nodes[op_idx].materialised, Some(true));
    }

    #[test]
    fn same_op_name_in_different_phases_classifies_per_phase() {
        // Two phases each declare an op named `select_ann`. One
        // version has metrics (→ Definitions → materialise); the
        // other doesn't (→ None → elide). The classifier MUST
        // resolve each scope-tree OpTemplate node against its
        // OWNING phase, not the first match by name across the
        // phases map.
        use nmbrs_workload::model::{MetricSpec, ParsedOp};
        let mut with_metrics = empty_phase();
        let mut op_with = ParsedOp::simple("select_ann", "noop");
        op_with.metrics.insert(
            "m".into(),
            MetricSpec {
                cell: Default::default(),
                value: "existing_wire".into(),
                family: None,
                kind: None,
                unit: None,
                format: None,
            },
        );
        with_metrics.ops.push(op_with);
        let mut without_metrics = empty_phase();
        without_metrics
            .ops
            .push(ParsedOp::simple("select_ann", "noop"));
        let mut phases = HashMap::new();
        phases.insert("ann_query".into(), with_metrics);
        phases.insert("pvs_metadata_query".into(), without_metrics);
        let mut tree = ScopeTree::build(
            "default",
            &[
                ScenarioNode::Phase("ann_query".into()),
                ScenarioNode::Phase("pvs_metadata_query".into()),
            ],
        );
        tree.extend_with_op_templates(&phases);
        mark_with(&mut tree, &BindingsDef::default(), &HashMap::new(), &phases);

        // Find the two select_ann op-template nodes, locate which
        // phase ancestor each lives under, and assert per-phase
        // materialisation.
        for (idx, node) in tree.iter_dfs() {
            let crate::scope_tree::ScopeKind::OpTemplate { name } = &node.kind else {
                continue;
            };
            if name != "select_ann" {
                continue;
            }
            // Walk up to the owning phase.
            let mut cursor = node.parent;
            let mut phase = None;
            while let Some(p) = cursor {
                if let crate::scope_tree::ScopeKind::Phase { name } = &tree.nodes[p].kind {
                    phase = Some(name.clone());
                    break;
                }
                cursor = tree.nodes[p].parent;
            }
            let expected_materialised = match phase.as_deref() {
                Some("ann_query") => Some(true),
                Some("pvs_metadata_query") => Some(false),
                other => panic!("unexpected owning phase: {other:?}"),
            };
            assert_eq!(
                tree.nodes[idx].materialised, expected_materialised,
                "op-template {name} under phase {phase:?} should materialise={expected_materialised:?}"
            );
        }
    }

    #[test]
    fn elision_summary_dumps_dfs_order() {
        let mut phases = HashMap::new();
        phases.insert("p".into(), empty_phase());
        let mut tree = ScopeTree::build("default", &[ScenarioNode::Phase("p".into())]);
        mark_with(&mut tree, &BindingsDef::default(), &HashMap::new(), &phases);
        let summary = elision_summary(&tree);
        // DFS pre-order: session → workload → scenario → phase. The
        // session root contributes no logical-path segment (SRD-88).
        assert_eq!(summary.len(), 4);
        assert_eq!(summary[0].3, "");
        assert_eq!(summary[1].3, "workload");
        assert_eq!(summary[2].3, "workload.scenario.default");
        assert_eq!(summary[3].3, "workload.scenario.default.phase.p");
        for (_, _, mat, _, _) in &summary {
            assert!(mat.is_some());
        }
    }
}
