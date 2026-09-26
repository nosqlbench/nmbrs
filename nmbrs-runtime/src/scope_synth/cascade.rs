// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Shared cascade walker — the single implementation of "given
//! a parent kernel, decide what cascade-extern and inline-const
//! lines to emit for a child scope's Polydat source."
//!
//! Pre-walker, each of the four sister scope builders
//! (`build_phase_scope_kernel`, `build_do_loop_scope_kernel`,
//! `build_op_template_scope_kernel`, and the forthcoming
//! `build_for_each_scope_kernel`) reinvented this walk with
//! subtly different precision. This module consolidates the
//! walk so:
//!
//! 1. The cascade rules live in one place. Bug fixes apply
//!    everywhere.
//! 2. The most-rigorous version (phase scope's SRD-13f
//!    provenance-aware inlining) becomes the canonical
//!    behavior for every scope kind.
//! 3. Scope-specific bits stay in the per-scope builder —
//!    each builder contributes only what's unique to it
//!    (body source, iter-var declarations, counter, etc.).
//!
//! ## What this walker is NOT for
//!
//! `build_op_template_scope_kernel` is intentionally **not**
//! refactored onto this walker. The op-template uses a
//! deliberately narrow "lazy cascade" policy — emit externs
//! only for names the op references, never a broad parent-
//! output / parent-input sweep. That keeps the op-template
//! kernel narrow at the cost of more bookkeeping in the
//! caller. Forcing it onto a broad-cascade walker would
//! either bloat the walker with op-template-specific knobs or
//! widen the op-template kernel; neither is desirable. The
//! op-template stays self-contained.
//!
//! ## What the walker does (in order)
//!
//! 1. **Coord-set detection.** Walks `parent.program().coord_count()`
//!    and `input_names()` to discover which names are parent-
//!    coord-slots — those propagate via the kernel chain, not via
//!    extern cascade. They're added to a skip set.
//! 2. **Local-inclusion-chain inline.** For each `referenced`
//!    name that resolves to a non-final cycle binding in the
//!    parent's AST (i.e., would have to be re-computed at this
//!    scope), pretty-prints the parent's inclusion chain and
//!    emits it as local matter. Names included this way are
//!    marked as `emitted`. (SRD-13f §"Wire-reference
//!    classification" case 3.)
//! 3. **Referenced-name cascade.** For each remaining
//!    `referenced` name, looks it up in `parent_manifest` for
//!    its typed port; emits `extern NAME: TYPE` and marks
//!    inherited.
//! 4. **Workload-params cascade.** For each entry in
//!    `workload_params` not yet emitted and not pre-emitted,
//!    emits `extern NAME: TYPE` (type detected from value
//!    shape). Marks inherited.
//! 5. **Parent-output cascade with provenance-aware inlining.**
//!    For each `parent.program().output_names()` not yet
//!    emitted: if the upstream's value is statically known
//!    (provenance == 0), inlines as `const NAME := <literal>`;
//!    otherwise cascades as `extern NAME: TYPE`. (SRD-13f
//!    §"Materialization gradient".)
//! 6. **Parent-input cascade.** For each
//!    `parent.program().input_names()` not yet emitted, emits
//!    `extern NAME: TYPE`. Closes the chain so cascade-extern'd
//!    inputs propagate transitively.
//!
//! ## What the walker does NOT do
//!
//! - Emit the scope's own body / iter-var / counter
//!   declarations — those come from the per-scope builder and
//!   are passed in as `pre_emitted` so the walker doesn't
//!   re-emit them.
//! - Call `finalize` — that's the per-scope builder's
//!   responsibility (it threads scope-specific `CompileOptions`
//!   and context labels).

use std::collections::{HashMap, HashSet};

use crate::scope_kernel::ScopeKernel;
use polydat::kernel::ManifestEntry;

use super::helpers::{
    format_value_as_final_literal, port_type_to_extern_name, workload_param_type_name,
};

/// Per-scope context the walker needs from its caller.
///
/// Each scope-kind builder constructs this with its own pre-
/// emitted set, referenced-placeholder set, and shadow-name
/// set, then passes it through to the walker.
pub struct CascadeInputs<'a> {
    /// Parent kernel — the chain root for all walks.
    pub parent_kernel: &'a ScopeKernel,
    /// Workload params (CLI / params: block defaults).
    pub workload_params: &'a HashMap<String, String>,
    /// Typed manifest of the parent's outputs — used to look up
    /// port types for referenced-name cascade. The runner
    /// already extracts this for the per-scope synthesisers;
    /// passing it in saves a re-walk.
    pub parent_manifest: &'a [ManifestEntry],
    /// Names referenced in the scope's body / spec exprs (the
    /// `{name}` placeholders + identifier scan of Polydat body
    /// source). Drives steps 2 + 3 of the walker.
    pub referenced: &'a HashSet<String>,
    /// Names already declared by the per-scope builder before
    /// the walker runs (iter vars, counter, body-locally-
    /// declared idents). The walker won't re-emit these.
    pub pre_emitted: &'a HashSet<String>,
    /// Additional names the per-scope builder wants to shadow
    /// from the parent-output cascade — body-locally-declared
    /// idents that aren't externs. Without this, the cascade
    /// could re-emit a name the body locally assigns, causing
    /// a duplicate-binding compile error.
    pub shadow_names: &'a HashSet<String>,
    /// Whether to run the referenced-name cascade pass (step
    /// 3 in the walker's order). do-loop and for-each opt in:
    /// their bodies are tightly scoped (a condition expression
    /// or a comprehension spec) where every referenced name is
    /// expected to flow in as an extern. phase opts out: its
    /// body is arbitrary Polydat source that may already declare
    /// the same names as `input` / `extern`, and a step-3
    /// emission would collide.
    pub include_referenced_cascade: bool,
}

/// Per-scope outputs the walker mutates.
pub struct CascadeOutputs<'a> {
    /// Polydat source string the walker appends to.
    pub source: &'a mut String,
    /// Names the walker has emitted, including any pre-emitted
    /// names that flowed in via [`CascadeInputs::pre_emitted`].
    /// Grows across the walk's six steps.
    pub emitted: &'a mut HashSet<String>,
    /// Names whose outputs are inherited from a parent (cascade-
    /// extern'd, not this scope's own iter-coord). The per-scope
    /// builder threads this into `PolydatMatter::inherited_outputs`
    /// so `compute_own_coordinates` excludes them when reporting
    /// this scope's own iteration position.
    pub inherited_names: &'a mut Vec<String>,
}

/// Run the shared cascade walk. See module docs for the six
/// steps and their order.
pub fn cascade_parent_into_source(inputs: CascadeInputs<'_>, outputs: CascadeOutputs<'_>) {
    let CascadeInputs {
        parent_kernel,
        workload_params,
        parent_manifest,
        referenced,
        pre_emitted,
        shadow_names,
        include_referenced_cascade,
    } = inputs;

    // Seed `emitted` with the caller's pre-emitted set so the
    // walker's skip predicates respect them.
    outputs.emitted.extend(pre_emitted.iter().cloned());

    let parent_program = parent_kernel.program();

    // Step 1 — coord-set detection.
    let coord_names: HashSet<String> = {
        let coord_count = parent_program.coord_count();
        parent_program
            .input_names()
            .into_iter()
            .take(coord_count)
            .collect()
    };

    // Step 2 — local-inclusion-chain inline.
    let already_satisfied_for_inclusion: HashSet<String> = pre_emitted
        .iter()
        .chain(coord_names.iter())
        .cloned()
        .collect();
    {
        let mut already_satisfied = already_satisfied_for_inclusion.clone();
        let mut refs_sorted: Vec<&String> = referenced.iter().collect();
        refs_sorted.sort();
        for name in refs_sorted {
            if already_satisfied.contains(name.as_str()) {
                continue;
            }
            // FINAL/SHARED go through the parent-output cascade
            // (step 5 handles their emission); only cycle
            // bindings reach the local-inclusion-chain path.
            let modifier = parent_program.output_modifier(name);
            if modifier == polydat::dsl::ast::BindingModifier::CONST
                || modifier == polydat::dsl::ast::BindingModifier::SHARED
            {
                continue;
            }
            let chain = parent_program.local_inclusion_chain(name, &already_satisfied);
            if chain.is_empty() {
                continue;
            }
            for stmt in chain {
                let line = polydat::dsl::pprint::pp_statement(stmt);
                outputs.source.push_str(&line);
                outputs.source.push('\n');
                if let polydat::dsl::ast::Statement::Binding(b) = stmt {
                    for t in &b.targets {
                        outputs.emitted.insert(t.clone());
                        already_satisfied.insert(t.clone());
                    }
                }
            }
        }
    }

    // Step 3 — referenced-name cascade against parent_manifest.
    // Opt-in per scope kind (see `include_referenced_cascade`
    // doc). Always skips coord names; the kernel chain
    // propagates those, not extern cascade.
    if include_referenced_cascade {
        let manifest_by_name: HashMap<&str, &ManifestEntry> = parent_manifest
            .iter()
            .map(|e| (e.name.as_str(), e))
            .collect();
        let mut refs_sorted: Vec<&String> = referenced.iter().collect();
        refs_sorted.sort();
        for name in refs_sorted {
            if outputs.emitted.contains(name) {
                continue;
            }
            if pre_emitted.contains(name) {
                continue;
            }
            if coord_names.contains(name) {
                continue;
            }
            if let Some(entry) = manifest_by_name.get(name.as_str()) {
                let type_name = port_type_to_extern_name(entry.port_type);
                outputs
                    .source
                    .push_str(&format!("extern {name}: {type_name}\n"));
                outputs.emitted.insert(name.clone());
                outputs.inherited_names.push(name.clone());
            } else if let Some(value) = workload_params.get(name) {
                super::cascade_emit::emit_workload_param_chain_aware(
                    name,
                    value,
                    parent_kernel,
                    outputs.source,
                    outputs.emitted,
                    None,
                );
            }
        }
    } else {
        // Suppress unused warning when this scope kind opts out.
        let _ = parent_manifest;
    }

    // Step 4 — workload-params cascade.
    for (name, value) in workload_params {
        if outputs.emitted.contains(name) {
            continue;
        }
        if shadow_names.contains(name) {
            continue;
        }
        let type_name = workload_param_type_name(value);
        outputs
            .source
            .push_str(&format!("extern {name}: {type_name}\n"));
        outputs.emitted.insert(name.clone());
        outputs.inherited_names.push(name.clone());
    }

    // Skip predicate for the parent-output / parent-input passes
    // — covers everything `emitted` so far + coord names +
    // internal underscore-prefixed names (with the cursor-extent
    // auxiliary exception).
    let skip_cascade = |emitted: &HashSet<String>, name: &str| -> bool {
        if emitted.contains(name) {
            return true;
        }
        if coord_names.contains(name) {
            return true;
        }
        if name.starts_with("__") && !name.starts_with("__cursor_extent_") {
            return true;
        }
        false
    };

    // Step 5 — parent-output cascade with provenance-aware
    // inlining.
    for name in parent_program.output_names() {
        let owned = name.to_string();
        if skip_cascade(outputs.emitted, &owned) {
            continue;
        }
        if shadow_names.contains(&owned) {
            continue;
        }
        // SRD-13f §"Materialization gradient" — only truly-
        // input-independent values (provenance == 0) are safe
        // to inline as literals into the cached downstream
        // program. Everything else cascades as `extern`; per-
        // activation materialization delivers the correct value.
        //
        // A `shared` output is NEVER statically known, whatever its
        // initializer's provenance: the cell is mutable at runtime
        // (captures write it). Const-inlining one freezes the
        // initializer AND makes the child the name's local final,
        // which `materialize_wiring_from_outer` honors by DROPPING
        // the ancestral cell — descendants then read the frozen
        // initializer forever and a writer's update is never
        // visible to them.
        let Some(output_idx) = parent_program.output_index(&owned) else {
            continue;
        };
        let (node_idx, port_idx) = parent_program.resolve_output_by_index(output_idx);
        let is_shared =
            parent_program.output_modifier(&owned) == polydat::dsl::ast::BindingModifier::SHARED;
        let upstream_is_statically_known = !is_shared
            && parent_program
                .input_provenance_for(node_idx)
                .is_none_or(|p| p.is_zero());
        if upstream_is_statically_known
            && let Some(value) = parent_kernel.lookup(&owned)
            && let Some(literal) = format_value_as_final_literal(&value)
        {
            outputs
                .source
                .push_str(&format!("const {owned} := {literal}\n"));
            outputs.emitted.insert(owned);
            continue;
        }
        let port_type = parent_program.node_meta(node_idx).outs[port_idx].typ;
        let type_name = port_type_to_extern_name(port_type);
        outputs
            .source
            .push_str(&format!("extern {owned}: {type_name}\n"));
        outputs.emitted.insert(owned.clone());
        outputs.inherited_names.push(owned);
    }

    // Step 6 — parent-input cascade.
    for name in parent_program.input_names() {
        if skip_cascade(outputs.emitted, &name) {
            continue;
        }
        if shadow_names.contains(&name) {
            continue;
        }
        // input_names() returns names that exist as inputs;
        // input_port_type on the same name therefore can't be
        // None unless the kernel's program shape is broken.
        let port_type = parent_program
            .input_port_type(&name)
            .expect("input_names() returned a name with no port type");
        let type_name = port_type.to_keyword();
        outputs
            .source
            .push_str(&format!("extern {name}: {type_name}\n"));
        outputs.emitted.insert(name.clone());
        outputs.inherited_names.push(name);
    }
}
