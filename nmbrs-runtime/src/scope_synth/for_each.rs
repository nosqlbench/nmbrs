// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! [`build_for_each_scope_kernel`] — synthesize the kernel for
//! a for-each scope.
//!
//! Activity-side replacement for
//! `polydat::iteration::comprehension::synthesize_for_each_scope`. The
//! comprehension-specific walking lives here; the broad
//! parent-program cascade is delegated to the shared
//! [`super::cascade::cascade_parent_into_source`] walker.
//!
//! ## What lives here (comprehension-specific)
//!
//! - Probe pre-evaluation of clause spec expressions to detect
//!   each iter-var's native type (so the emitted extern uses
//!   the right `u64` / `f64` / `Str` / `Bool` / `Ext`).
//! - Cross-clause placeholder discovery — earlier iter-vars'
//!   probe values may substitute into later clauses' spec
//!   expressions, surfacing additional names the cascade
//!   needs to resolve.
//! - Phase-bindings appendage (SRD-13f Push E) — when a phase
//!   carries both `for_each:` and `bindings:`, the bindings
//!   source lives on the for-each scope's kernel.
//!
//! ## What's delegated (generic cascade)
//!
//! Everything else — parent-output cascade with provenance-
//! aware inlining, parent-input cascade, workload-params
//! cascade, local-inclusion-chain inlining — runs through the
//! shared walker.

use std::collections::HashSet;

use crate::scope_kernel::ScopeKernel;
use polydat::iteration::comprehension::pre_evaluate_clause;
use polydat::kernel::ManifestEntry;

use super::cascade::{CascadeInputs, CascadeOutputs, cascade_parent_into_source};
use super::helpers::{collect_leaf_placeholders, scan_one};

/// Synthesize and compile a Polydat Kernel for a for-each scope.
///
/// `bindings` is `[(iter_var, spec_expr)]` per scalar variable
/// (parallel-iter clauses contribute one entry per scalar).
/// `parent_manifest` describes the parent kernel's typed
/// outputs (use `polydat::kernel::extract_manifest` on the
/// parent's program). `parent_kernel` provides the in-scope
/// name space for clause pre-evaluation.
/// `phase_bindings` is optional Polydat source folded in after the
/// extern cascade (SRD-13f Push E — when a phase declares both
/// `for_each:` and `bindings:`, the bindings live on this scope).
///
/// Returns a kernel with:
/// - One extern per iter-var, typed via probe pre-evaluation.
/// - Cascade-extern declarations for every parent-visible
///   name the spec expressions reference and the parent
///   exposes.
/// - `materialize_wiring_from_outer(parent)` already called.
/// - Parent input-slot values propagated via
///   `polydat::kernel::propagate_inputs` ([`ScopeKernel::synthesize_under`]).
///
/// The caller's responsibility: per-iteration, install the
/// tuple's typed values on this kernel's input slots before
/// evaluating children.
#[allow(clippy::too_many_arguments)]
pub fn build_for_each_scope_kernel(
    bindings: &[(String, String)],
    parent_manifest: &[ManifestEntry],
    parent_kernel: &ScopeKernel,
    workload_params: &std::collections::HashMap<String, String>,
    polydat_lib_paths: Vec<std::path::PathBuf>,
    workload_dir: Option<&std::path::Path>,
    strict: bool,
    context: &str,
    phase_bindings: Option<&str>,
) -> Result<ScopeKernel, String> {
    let iter_vars: Vec<String> = bindings.iter().map(|(v, _)| v.clone()).collect();
    let spec_exprs: Vec<String> = bindings.iter().map(|(_, e)| e.clone()).collect();

    let mut source = String::new();
    let mut emitted: HashSet<String> = HashSet::new();
    let mut inherited_names: Vec<String> = Vec::new();

    // Probe each clause's spec expression to detect the iter-
    // var's native Polydat type and discover any additional
    // placeholders that surface after earlier-iter-vars'
    // values substitute into later specs.
    //
    // `probes` holds the first probe-value per iter-var (used
    // for cross-clause substitution); `all_referenced` accumulates
    // every placeholder discovered along the way.
    let mut probes: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut all_referenced: HashSet<String> = collect_leaf_placeholders(&spec_exprs);
    for (idx, var) in iter_vars.iter().enumerate() {
        if emitted.contains(var) {
            continue;
        }
        let spec_text = spec_exprs.get(idx).map(String::as_str).unwrap_or("");
        let values = pre_evaluate_clause(spec_text, parent_kernel, workload_params, &probes)
            .unwrap_or_default();
        let detected_type = values
            .first()
            .map(polydat::iteration::comprehension::value_to_polydat_type_name)
            .unwrap_or("String");
        source.push_str(&format!("extern {var}: {detected_type}\n"));
        emitted.insert(var.clone());

        // Discover placeholders that emerge after substituting
        // this iter-var's first value into later specs.
        for v in &values {
            let v_str = v.to_display_string();
            for next_spec in &spec_exprs[idx + 1..] {
                let mut substituted = next_spec.clone();
                substituted = substituted.replace(&format!("{{{var}}}"), &v_str);
                let mut emergent = HashSet::new();
                scan_one(&substituted, &mut emergent);
                all_referenced.extend(emergent);
            }
        }
        if let Some(first) = values.into_iter().next() {
            probes.insert(var.clone(), first.to_display_string());
        }
    }

    // The iter-vars are pre-emitted; we don't want the cascade
    // to re-emit them.
    let pre_emitted: HashSet<String> = iter_vars.iter().cloned().collect();
    // Drive the shared cascade walker. include_referenced_cascade
    // is true for for_each — its spec expressions are narrow
    // Polydat source where every referenced name needs an extern.
    cascade_parent_into_source(
        CascadeInputs {
            parent_kernel,
            workload_params,
            parent_manifest,
            referenced: &all_referenced,
            pre_emitted: &pre_emitted,
            shadow_names: &pre_emitted,
            include_referenced_cascade: true,
        },
        CascadeOutputs {
            source: &mut source,
            emitted: &mut emitted,
            inherited_names: &mut inherited_names,
        },
    );

    // SRD-13f Push E — append phase-level `bindings:` source
    // after the extern cascade. Phase bindings can reference
    // iter vars (now externs above) and any cascaded parent
    // name; the Polydat compiler resolves both.
    if let Some(body) = phase_bindings {
        let trimmed = body.trim();
        if !trimmed.is_empty() {
            if !source.ends_with('\n') && !source.is_empty() {
                source.push('\n');
            }
            source.push_str(body);
            if !source.ends_with('\n') {
                source.push('\n');
            }
        }
    }

    if source.is_empty() {
        source.push_str("const __empty := 0\n");
    }

    // SRD-67 Phase 3 — finalize through the SubcontextBuilder
    // bridge. The for_each synthesiser threads polydat_lib_paths /
    // workload_dir / strict through CompileOptions so the
    // underlying compile invocation matches the legacy call.
    let compile_options = polydat::kernel::subcontext::CompileOptions {
        workload_dir: workload_dir.map(|p| p.to_path_buf()),
        polydat_lib_paths,
        strict,
        required_outputs: Vec::new(),
        context_label: Some(context.to_string()),
        cursor_limit: None,
        ..Default::default()
    };
    crate::scope_kernel::ScopeKernel::synthesize_under(
        parent_kernel,
        crate::scope_kernel::SourceMatter::source(context, source, compile_options)
            .inherited(inherited_names),
    )
    .map_err(|e| format!("{context}: for_each scope synthesis: {e}"))
}
