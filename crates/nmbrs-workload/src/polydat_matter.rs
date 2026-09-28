// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-13d §3.1 — declarative GK-content classification for
//! every node in the workload AST. The scope-tree pre-walker
//! uses [`HasPolydatMatter::polydat_matter`] as the **first question** at
//! every scope decision: most workloads short-circuit here
//! and never reach the program-hash-equivalence refinement
//! (§3.2).
//!
//! Implementations cover the AST types — runtime objects
//! (the `Component` tree, fibers, dispensers) consume the
//! marks the trait produced; they don't implement the trait
//! themselves. Polydat content lives on the AST, not on runtime
//! state.

use crate::model::{BindingsDef, ParsedOp, ScenarioNode, Workload, WorkloadPhase};

/// SRD-13d §3.1 classification of how much Polydat content a
/// scope-tree node carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolydatMatter {
    /// No Polydat references at all — no `bindings:`, no
    /// `metrics:`, no inline `{{<expr>}}`, no GK-typed
    /// fields. Walker skips kernel construction entirely.
    None,
    /// References parent-scope Polydat names but **defines
    /// nothing new**. Examples: `metrics:` declarations whose
    /// `value:` is a bare name resolving to a parent binding;
    /// inline `{{<name>}}` substitution where `<name>` is a
    /// parent binding; op fields that bind parent-scope
    /// wires without declaring new ones. Walker skips kernel
    /// construction; reads thread through the parent's
    /// kernel state directly.
    Readonly,
    /// Declares new bindings, wire expressions, or constants
    /// that the parent doesn't supply. Walker materialises a
    /// kernel for this node — possibly subject to hash-check
    /// flattening (§3.2) if the new content turns out to be
    /// equivalent to the parent's.
    Definitions,
}

/// Implemented by every workload-AST type that can sit in
/// the construction tree. Pure function of the parsed AST;
/// no runtime state, no compilation.
pub trait HasPolydatMatter {
    /// Classify this node's contribution to Polydat content.
    fn polydat_matter(&self) -> PolydatMatter;
}

// -----------------------------------------------------------
// Helpers
// -----------------------------------------------------------

/// `bindings:` block contributes definitions when non-empty.
fn bindings_def_matter(b: &BindingsDef) -> PolydatMatter {
    if b.is_empty() {
        PolydatMatter::None
    } else {
        PolydatMatter::Definitions
    }
}

/// True when any value field on the op uses inline `{{<expr>}}`
/// substitution. Promotes to `Definitions` because the
/// rewrite pass (`crate::scope::rewrite_inline_exprs` in
/// nmbrs-runtime) hoists each into a `__expr_N := <expr>`
/// binding owned by the op.
fn has_inline_expr(op: &ParsedOp) -> bool {
    fn scan(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::String(s) => s.contains("{{") && s.contains("}}"),
            serde_json::Value::Array(arr) => arr.iter().any(scan),
            serde_json::Value::Object(map) => map.values().any(scan),
            _ => false,
        }
    }
    op.op.values().any(scan) || op.params.values().any(scan)
}

// -----------------------------------------------------------
// Trait impls
// -----------------------------------------------------------

impl HasPolydatMatter for ParsedOp {
    fn polydat_matter(&self) -> PolydatMatter {
        // Op-level bindings always promote when non-empty.
        let by_bindings = bindings_def_matter(&self.bindings);
        if by_bindings == PolydatMatter::Definitions {
            return PolydatMatter::Definitions;
        }

        // Inline `{{<expr>}}` constructs on any op field
        // hoist into anonymous bindings during pre-compile.
        if has_inline_expr(self) {
            return PolydatMatter::Definitions;
        }

        // metrics: any declared metric contributes Definitions.
        // Post-SRD-68 follow-up: the op-template synthesiser
        // appends a `__metric_<name> := <value_expr>` binding per
        // metric (see `crate::scope::synthesize_metric_binding_name`)
        // so the closure-binding economy can walk the value
        // expression's free identifiers for magic-extern slot
        // allocation. That synthesised binding is itself a
        // definition, so even a bare-name `value: count` requires
        // an op-template kernel — the kernel is where the
        // synthesised `__metric_<name>` LHS lands.
        if !self.metrics.is_empty() {
            return PolydatMatter::Definitions;
        }

        // result: declarations expose result-body fields as
        // Polydat wires — a definition by construction (the
        // wire didn't exist before this op).
        if self.result.as_ref().is_some_and(|r| !r.is_empty()) {
            return PolydatMatter::Definitions;
        }

        // capture: writes result-extracted values onto Polydat
        // wires — definitions by construction, same as `result:`.
        // The op-template kernel is where the capture target's
        // slot lives; when the target is an ancestral `shared`
        // cell, that slot is what the wiring cascade cell-binds.
        // Without materialisation the cycle-time write lands on a
        // non-cell fallback and the captured value never crosses
        // the phase boundary — a later phase gating on the cell
        // reads its initializer forever.
        if !self.captures.is_empty() {
            return PolydatMatter::Definitions;
        }

        // Anything that gets here reads parent bindings only
        // (e.g. `if:` / `delay:` references) or has nothing
        // GK-shaped at all.
        if self.condition.is_some() || self.delay.is_some() {
            PolydatMatter::Readonly
        } else {
            PolydatMatter::None
        }
    }
}

impl HasPolydatMatter for WorkloadPhase {
    fn polydat_matter(&self) -> PolydatMatter {
        // Phase-level `bindings:` block on the phase AST.
        // Today's parser also legacy-merges this into per-op
        // bindings; the phase still owns the structural fact
        // that it declared the binding (SRD-13d §3.1's
        // classification operates on the AST, not on the
        // post-merge runtime view).
        let by_bindings = bindings_def_matter(&self.bindings);
        if by_bindings == PolydatMatter::Definitions {
            return PolydatMatter::Definitions;
        }
        // Phase-level `metrics:` synthesise `volatile __metric_<name>
        // := <value>` (plus the injected `phase_start` extern) onto
        // the phase kernel — definitions by construction, so the
        // phase needs its own scope kernel even with no `bindings:`.
        if !self.metrics.is_empty() {
            return PolydatMatter::Definitions;
        }
        // `for_each:` clauses always bind iteration variables.
        if self.for_each.is_some() {
            return PolydatMatter::Definitions;
        }
        // `cycles` / `concurrency` referencing workload-param
        // Polydat names (`{train_count}` etc.) ⇒ Readonly. `rate`
        // is f64-typed today (no Polydat refs) but counted as a
        // parent reference for symmetry; revisit when rate
        // grows GK-expression support.
        let refs_parent = [&self.cycles, &self.concurrency].iter().any(|opt| {
            opt.as_ref()
                .is_some_and(|s| s.contains('{') && s.contains('}'))
        });
        if refs_parent || self.rate.is_some() {
            return PolydatMatter::Readonly;
        }
        PolydatMatter::None
    }
}

impl HasPolydatMatter for ScenarioNode {
    fn polydat_matter(&self) -> PolydatMatter {
        match self {
            // Iteration constructs always declare iteration
            // variables — Definitions by construction.
            ScenarioNode::Comprehension { .. }
            | ScenarioNode::DoWhile { .. }
            | ScenarioNode::DoUntil { .. } => PolydatMatter::Definitions,
            // Phase reference + scenario-include wrappers
            // don't add Polydat content on their own; the
            // wrapped phase / included scenario carries it.
            ScenarioNode::Phase(_) | ScenarioNode::IncludedScenario { .. } => PolydatMatter::None,
            // Scenario-tree-level `bindings:` block (also the
            // canonical lowered form of `set:` sugar) installs
            // a Polydat scope kernel — Definitions by definition.
            ScenarioNode::Bindings { .. } => PolydatMatter::Definitions,
        }
    }
}

impl HasPolydatMatter for Workload {
    fn polydat_matter(&self) -> PolydatMatter {
        // Workload root carries the top-level `bindings:`
        // block + workload-level params. Either contributes
        // Definitions when non-empty.
        let by_bindings = bindings_def_matter(&self.bindings);
        if by_bindings == PolydatMatter::Definitions {
            return PolydatMatter::Definitions;
        }
        if !self.params.is_empty() {
            // Params turn into `final <name> := <literal>`
            // bindings on the workload-params kernel
            // (crates/nmbrs-runtime/src/params.rs), so any param
            // declaration is Polydat content.
            return PolydatMatter::Definitions;
        }
        PolydatMatter::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MetricSpec, ResultSpec};

    fn empty_op(name: &str) -> ParsedOp {
        ParsedOp::simple(name, "noop")
    }

    // ── ParsedOp ─────────────────────────────────────────

    #[test]
    fn parsed_op_with_no_polydat_content_is_none() {
        let op = empty_op("x");
        assert_eq!(op.polydat_matter(), PolydatMatter::None);
    }

    #[test]
    fn parsed_op_with_bindings_is_definitions() {
        let mut op = empty_op("x");
        op.bindings = BindingsDef::PolydatSource("k := 5".into());
        assert_eq!(op.polydat_matter(), PolydatMatter::Definitions);
    }

    #[test]
    fn parsed_op_empty_bindings_string_is_none() {
        let mut op = empty_op("x");
        op.bindings = BindingsDef::PolydatSource("   \n  ".into());
        assert_eq!(op.polydat_matter(), PolydatMatter::None);
    }

    #[test]
    fn parsed_op_with_inline_expr_is_definitions() {
        let mut op = empty_op("x");
        op.op.insert(
            "stmt".into(),
            serde_json::Value::String("SELECT {{cycle}}".into()),
        );
        assert_eq!(op.polydat_matter(), PolydatMatter::Definitions);
    }

    #[test]
    fn parsed_op_metrics_bare_value_is_definitions() {
        // Post-SRD-68 follow-up: every metric synthesises a
        // `__metric_<name> := <value>` binding into the
        // op-template kernel — even bare-name `value:` forms
        // become a definition (the synthesised LHS is the
        // definition), which requires an op-template kernel
        // to exist. Used to return Readonly back when
        // MetricsDispenser read parent bindings directly;
        // that path is gone.
        let mut op = empty_op("x");
        op.metrics.insert(
            "foo".into(),
            MetricSpec {
                value: "existing_wire".into(),
                family: None,
                kind: None,
                unit: None,
                format: None,
                cell: Default::default(),
            },
        );
        assert_eq!(op.polydat_matter(), PolydatMatter::Definitions);
    }

    #[test]
    fn parsed_op_metrics_dotted_name_is_definitions() {
        // Same as above; dotted-name forms equally require a
        // synthesised `__metric_<name>` binding.
        let mut op = empty_op("x");
        op.metrics.insert(
            "foo".into(),
            MetricSpec {
                value: "phase.recall_at_10".into(),
                family: None,
                kind: None,
                unit: None,
                format: None,
                cell: Default::default(),
            },
        );
        assert_eq!(op.polydat_matter(), PolydatMatter::Definitions);
    }

    #[test]
    fn parsed_op_metrics_expression_is_definitions() {
        let mut op = empty_op("x");
        op.metrics.insert(
            "foo".into(),
            MetricSpec {
                value: "factor * 2.0".into(),
                family: None,
                kind: None,
                unit: None,
                format: None,
                cell: Default::default(),
            },
        );
        assert_eq!(op.polydat_matter(), PolydatMatter::Definitions);
    }

    #[test]
    fn parsed_op_with_result_is_definitions() {
        let mut op = empty_op("x");
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("rows_returned".into(), "count".into());
        op.result = Some(ResultSpec::Map(entries));
        assert_eq!(op.polydat_matter(), PolydatMatter::Definitions);
    }

    #[test]
    fn parsed_op_with_only_condition_is_readonly() {
        let mut op = empty_op("x");
        op.condition = Some("ok".into());
        assert_eq!(op.polydat_matter(), PolydatMatter::Readonly);
    }

    // ── ScenarioNode ─────────────────────────────────────

    // ── WorkloadPhase ───────────────────────────────────

    #[test]
    fn workload_phase_with_phase_bindings_is_definitions() {
        let phase = WorkloadPhase {
            cycles: None,
            concurrency: None,
            rate: None,
            adapter: None,
            errors: None,
            tags: None,
            ops: vec![],
            for_each: None,
            loop_scope: None,
            iter_scope: None,
            checkpoint: None,
            status_metrics: vec![],
            metrics: Default::default(),
            bindings: BindingsDef::PolydatSource("k := 5".into()),
            poll: None,
            ..Default::default()
        };
        assert_eq!(phase.polydat_matter(), PolydatMatter::Definitions);
    }

    #[test]
    fn workload_phase_with_for_each_is_definitions() {
        let phase = WorkloadPhase {
            cycles: None,
            concurrency: None,
            rate: None,
            adapter: None,
            errors: None,
            tags: None,
            ops: vec![],
            for_each: Some("k in 1,2,3".into()),
            loop_scope: None,
            iter_scope: None,
            checkpoint: None,
            status_metrics: vec![],
            metrics: Default::default(),
            bindings: BindingsDef::default(),
            poll: None,
            ..Default::default()
        };
        assert_eq!(phase.polydat_matter(), PolydatMatter::Definitions);
    }

    #[test]
    fn workload_phase_with_metrics_is_definitions() {
        // A phase-level `metrics:` block synthesises
        // `volatile __metric_<name> := <value>` (+ the injected
        // `phase_start` extern) onto the phase kernel, so the phase
        // needs its own scope kernel even with no `bindings:`.
        let mut metrics = std::collections::HashMap::new();
        metrics.insert(
            "time_to_index".to_string(),
            crate::model::MetricSpec {
                value: "current_epoch_millis() - phase_start".into(),
                family: None,
                kind: None,
                unit: None,
                format: None,
                cell: Default::default(),
            },
        );
        let phase = WorkloadPhase {
            cycles: None,
            concurrency: None,
            rate: None,
            adapter: None,
            errors: None,
            tags: None,
            ops: vec![],
            for_each: None,
            loop_scope: None,
            iter_scope: None,
            checkpoint: None,
            status_metrics: vec![],
            metrics,
            bindings: BindingsDef::default(),
            poll: None,
            ..Default::default()
        };
        assert_eq!(phase.polydat_matter(), PolydatMatter::Definitions);
    }

    #[test]
    fn workload_phase_bare_is_none() {
        let phase = WorkloadPhase {
            cycles: None,
            concurrency: None,
            rate: None,
            adapter: None,
            errors: None,
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
            ..Default::default()
        };
        assert_eq!(phase.polydat_matter(), PolydatMatter::None);
    }

    #[test]
    fn workload_phase_cycles_param_ref_is_readonly() {
        let phase = WorkloadPhase {
            cycles: Some("{train_count}".into()),
            concurrency: None,
            rate: None,
            adapter: None,
            errors: None,
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
            ..Default::default()
        };
        assert_eq!(phase.polydat_matter(), PolydatMatter::Readonly);
    }

    // ── ScenarioNode ─────────────────────────────────────

    #[test]
    fn scenario_node_phase_is_none() {
        let node = ScenarioNode::Phase("p".into());
        assert_eq!(node.polydat_matter(), PolydatMatter::None);
    }

    #[test]
    fn scenario_node_do_while_is_definitions() {
        let node = ScenarioNode::DoWhile {
            condition: "ok".into(),
            counter: None,
            children: vec![],
        };
        assert_eq!(node.polydat_matter(), PolydatMatter::Definitions);
    }
}
