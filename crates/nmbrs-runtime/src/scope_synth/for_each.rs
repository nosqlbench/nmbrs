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
//!   the right `u64` / `f64` / `str` / `bool` / `ext`).
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
        // A clause that fails to probe is an error with its own
        // diagnostic — unless it reads a name the parent declares
        // but can't value yet (an outer for_each's iter-var), which
        // only gets a value per iteration. See
        // [`probe_waits_on_outer_iteration`].
        let values = match pre_evaluate_clause(spec_text, parent_kernel, workload_params, &probes) {
            Ok(values) => values,
            Err(_)
                if probe_waits_on_outer_iteration(
                    spec_text,
                    parent_kernel,
                    workload_params,
                    &probes,
                ) =>
            {
                Vec::new()
            }
            Err(e) => return Err(format!("{context}: for_each `{var} in {spec_text}`: {e}")),
        };
        // No probe value — an empty source (zero iterations) or one
        // that waits on an outer iteration — declares `str`: the
        // clause's text is interpolated per iteration, so its
        // values arrive as strings.
        let detected_type = values
            .first()
            .map(polydat::ast::Value::port_type)
            .unwrap_or(polydat::ast::PortType::Str);
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

/// Whether a clause that failed to probe is waiting on an outer
/// iteration rather than wrong: it references (as a `{name}`
/// placeholder or as a bare-identifier source) a name the parent
/// kernel declares but has no value for at synthesis, and that no
/// earlier clause probe or workload param supplies. That is an
/// outer for_each's iter-var: its value is installed per
/// iteration, so the clause can only be evaluated then.
///
/// Any other failure — a malformed spec, a name nothing declares —
/// is not deferred, and the caller reports it.
fn probe_waits_on_outer_iteration(
    spec_text: &str,
    parent_kernel: &ScopeKernel,
    workload_params: &std::collections::HashMap<String, String>,
    probes: &std::collections::HashMap<String, String>,
) -> bool {
    let mut names = collect_leaf_placeholders(&[spec_text.to_string()]);
    let bare = spec_text.trim();
    if !bare.is_empty() && bare.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        names.insert(bare.to_string());
    }
    let kernel = parent_kernel.kernel();
    names.iter().any(|name| {
        !probes.contains_key(name)
            && !workload_params.contains_key(name)
            && parent_kernel.lookup(name).is_none()
            && (kernel.input_port_type(name).is_some() || kernel.output_type(name).is_some())
    })
}

#[cfg(test)]
mod tests {
    use super::build_for_each_scope_kernel;
    use crate::scope_kernel::ScopeKernel;
    use polydat::ast::PortType;
    use std::collections::HashMap;

    fn synth(parent: &ScopeKernel, var: &str, spec: &str) -> Result<ScopeKernel, String> {
        build_for_each_scope_kernel(
            &[(var.to_string(), spec.to_string())],
            &[],
            parent,
            &HashMap::new(),
            Vec::new(),
            None,
            false,
            "test_for_each",
            None,
        )
    }

    /// A source nothing declares fails at synthesis with the clause
    /// and polydat's own diagnostic, instead of typing the iter-var
    /// as a string and failing later on a type mismatch.
    #[test]
    fn unresolvable_clause_fails_with_its_own_diagnostic() {
        let parent = ScopeKernel::compile("\n").unwrap();
        for (spec, why) in [
            ("nope_values", "did not resolve"),
            ("pre_{nobody}", "unresolved placeholder"),
        ] {
            let err = synth(&parent, "k", spec).expect_err("must fail at synthesis");
            assert!(
                err.contains(&format!("for_each `k in {spec}`")) && err.contains(why),
                "{spec}: {err}"
            );
        }
    }

    /// A clause reading an outer for_each's iter-var — declared by
    /// the parent, valued only per iteration — is deferred, and its
    /// iter-var is declared `str`.
    #[test]
    fn clause_on_an_outer_iter_var_waits_for_the_iteration() {
        let parent = ScopeKernel::compile("extern outer: str\n").unwrap();
        let kernel = synth(&parent, "inner", "pre_{outer}").expect("deferred, not an error");
        assert_eq!(
            kernel.kernel().input_port_type("inner"),
            Some(PortType::Str)
        );
    }
}
