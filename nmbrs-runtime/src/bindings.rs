// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Binding expression compiler: parses nosqlbench-style binding chains
//! into Polydat Kernel node wiring.
//!
//! Binding syntax: `FuncA(args); FuncB(args); FuncC(args)`
//!
//! This is a semicolon-delimited chain where the output of each
//! function feeds the input of the next, starting from the `cycle`
//! coordinate. So `Hash(); Mod(1000000)` becomes:
//!
//! ```text
//! __binding_h_0 := hash(cycle)
//! binding_name  := mod(__binding_h_0, 1000000)
//! ```

use std::collections::{HashMap, HashSet};

use polydat::kernel::PolydatKernel;

use nmbrs_workload::bindpoints;
use nmbrs_workload::model::ParsedOp;

/// A parsed function call in a binding chain.
#[derive(Debug, Clone)]
struct BindingFunc {
    name: String,
    args: Vec<String>,
}

/// Parse a binding expression into a chain of function calls.
///
/// `"Hash(); Mod(1000000)"` → `[BindingFunc{name:"Hash", args:[]}, BindingFunc{name:"Mod", args:["1000000"]}]`
fn parse_binding_chain(expr: &str) -> Vec<BindingFunc> {
    let mut funcs = Vec::new();

    for segment in expr.split(';') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }

        // Find function name and args
        if let Some(paren_pos) = segment.find('(') {
            let name = segment[..paren_pos].trim().to_string();
            let args_str = &segment[paren_pos + 1..];
            let args_str = args_str.trim_end_matches(')').trim();

            let args: Vec<String> = if args_str.is_empty() {
                Vec::new()
            } else {
                // Split on commas, respecting nested parens
                split_args(args_str)
            };

            funcs.push(BindingFunc { name, args });
        } else {
            // No parens — treat as a nullary function
            funcs.push(BindingFunc {
                name: segment.trim().to_string(),
                args: Vec::new(),
            });
        }
    }

    funcs
}

/// Split comma-separated arguments, respecting nested parentheses and quotes.
fn split_args(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut depth = 0;
    let mut in_quote = false;

    for c in s.chars() {
        match c {
            '\'' if !in_quote => {
                in_quote = true;
                current.push(c);
            }
            '\'' if in_quote => {
                in_quote = false;
                current.push(c);
            }
            '(' if !in_quote => {
                depth += 1;
                current.push(c);
            }
            ')' if !in_quote => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 && !in_quote => {
                args.push(current.trim().to_string());
                current = String::new();
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        args.push(current.trim().to_string());
    }
    args
}

// build_chain_node and its helpers have been removed.
// Legacy semicolon-chain bindings are now translated to Polydat source
// and compiled through the unified Polydat compiler. See compile_bindings_with_opts.

/// Compile all bindings from a set of ParsedOps into a Polydat Kernel.
///
/// Collects all unique binding names and expressions, plus any
/// unreferenced bind points in op fields (auto-bound to hash+mod).
/// Wires them through the Polydat assembler as proper node chains.
/// Probe the compile level of a Polydat function by name.
///
/// Instantiates a dummy node and calls its intrinsic `compile_level()`.
/// This is the single source of truth — no external classification needed.
/// Probe the compile level of a Polydat function by name.
///
/// Instantiates a node with a representative constant and probes
/// its intrinsic compile level. This is the single source of truth.
/// Probe the compile level of a Polydat function by name.
///
/// Constructs a representative Polydat program containing the function
/// and inspects the resulting kernel's node for its compile level.
/// Uses the unified Polydat compiler — no separate dispatch table.
pub fn probe_compile_level(func_name: &str) -> polydat::ast::CompileLevel {
    let sig = match polydat::dsl::registry::lookup(func_name) {
        Some(s) => s,
        None => return polydat::ast::CompileLevel::Phase1,
    };

    // Build args from per-parameter example values declared in FuncSig.
    // Wire params use their example (typically "cycle"), const params
    // use representative values that pass the node's validation.
    let mut parts = Vec::new();
    let mut has_wire = false;
    for p in sig.params {
        parts.push(p.example.to_string());
        if p.slot_type.is_wire() {
            has_wire = true;
        }
    }
    // Ensure at least one wire input for the coordinates declaration.
    if !has_wire && parts.is_empty() {
        parts.push("cycle".to_string());
    }

    let source = format!("input cycle: u64\nout := {func_name}({})", parts.join(", "));

    // Probe compile level via catch_unwind — fallback is Phase1.
    // Does not replace the global panic hook (not thread-safe).
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        polydat::dsl::compile_polydat(&source)
    }));

    match result {
        Ok(Ok(kernel)) => kernel.program().last_node_compile_level(),
        _ => polydat::ast::CompileLevel::Phase1,
    }
}

pub fn compile_bindings(ops: &[ParsedOp]) -> Result<PolydatKernel, String> {
    compile_bindings_with_path(ops, None)
}

/// Scan op params for bind point references (recursively through JSON values).
fn collect_param_bindings(
    params: &HashMap<String, serde_json::Value>,
    exclude: &[String],
    required: &mut Vec<String>,
) {
    for (key, value) in params.iter() {
        // `gutter:` templates resolve at RUNTIME — during-forms on the
        // fiber wires, `final:` at phase end with a status-metric
        // fallback (`{recall}`, `{latency_p50}`) that has no wire
        // declaration anywhere. Unresolved names degrade to visible
        // literal text in the cell, so they must not be collected as
        // required wire references.
        if key == "gutter" {
            continue;
        }
        collect_json_bindings(value, exclude, required);
    }
}

/// Public wrapper for `collect_param_bindings`, used by `scope.rs`.
pub fn collect_param_bindings_into(
    params: &HashMap<String, serde_json::Value>,
    exclude: &[String],
    required: &mut Vec<String>,
) {
    collect_param_bindings(params, exclude, required);
}

fn collect_json_bindings(
    value: &serde_json::Value,
    exclude: &[String],
    required: &mut Vec<String>,
) {
    match value {
        serde_json::Value::String(s) => {
            for name in bindpoints::referenced_bindings(s) {
                if !required.contains(&name) && !exclude.contains(&name) {
                    required.push(name);
                }
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                collect_json_bindings(v, exclude, required);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_json_bindings(v, exclude, required);
            }
        }
        _ => {}
    }
}

pub fn compile_bindings_with_path(
    ops: &[ParsedOp],
    source_dir: Option<&std::path::Path>,
) -> Result<PolydatKernel, String> {
    compile_bindings_with_opts(ops, source_dir, false)
}

/// Compile a Polydat Kernel from a pre-built `BindingScope`.
///
/// The scope has already been validated and carries structured
/// provenance. This function emits the scope to Polydat source, collects
/// required outputs, and compiles via the standard Polydat compiler.
///
/// `pragmas` carries the chain-walked effective pragma state for
/// the scope (typically obtained from
/// `ScopeTree::nodes[idx].pragmas`). Pragma directives matching
/// the effective state are prepended to the emitted source so
/// the Polydat compiler's existing AST-pragma extraction (SRD 15
/// §"Module-Level Pragmas") drives the assembler's strict-wire
/// flags. Pass `&PragmaSet::default()` to disable pragma effects
/// for legacy callers.
pub fn compile_from_scope(
    scope: &crate::scope::BindingScope,
    source_dir: Option<&std::path::Path>,
    polydat_lib_paths: Vec<std::path::PathBuf>,
    strict: bool,
    context: &str,
    cursor_limit: Option<u64>,
    pragmas: &polydat::dsl::pragmas::PragmaSet,
) -> Result<PolydatKernel, String> {
    let body = scope.emit();
    let required = scope.required_outputs();
    let source = prepend_effective_pragmas(pragmas, &body);
    let options = polydat::dsl::compile::CompileOptions {
        source_dir: source_dir.map(std::path::Path::to_path_buf),
        lib_paths: polydat_lib_paths,
        required_outputs: required,
        strict,
        context: context.to_string(),
        cursor_limit,
    };
    polydat::dsl::compile::compile_polydat_with_options(&source, &options, None)
}

/// Prepend pragma directives matching the chain's effective
/// state. The resulting Polydat source is functionally equivalent to
/// having the pragmas declared locally — the compiler's AST walk
/// picks them up the same way regardless of whether they came
/// from the original source or were synthesised here.
///
/// Provenance (which scope originally declared the pragma) is
/// flattened by this textualization. That's acceptable for the
/// runtime-effect path; diagnostic uses query the scope tree
/// directly via `ScopeTree::ancestors` for path labels.
pub(crate) fn prepend_effective_pragmas(
    pragmas: &polydat::dsl::pragmas::PragmaSet,
    body: &str,
) -> String {
    let mut out = String::new();
    if pragmas.strict_types() && pragmas.strict_values() {
        out.push_str("pragma strict\n");
    } else if pragmas.strict_types() {
        out.push_str("pragma strict_types\n");
    } else if pragmas.strict_values() {
        out.push_str("pragma strict_values\n");
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(body);
    out
}

/// Build the workload-root [`PolydatKernel`] as a subscope of the
/// workload-params kernel.
///
/// The workload-root is "just another scope" per SRD-67
/// §"Composition with SRD-66": it goes through the same
/// `parent.build_subscope(matter)` protocol every other scope
/// uses. The specialised content for the root is:
///
/// - Op-level bindings (rare at root; most workloads put
///   bindings at the workload or phase level).
/// - The workload's `bindings:` block, ingested as Inherited
///   matter.
/// - Workload params as `const` bindings — descendants pick
///   them up via the standard manifest auto-extern.
/// - DCE filter that retains every workload param plus
///   caller-supplied config refs (`cycles=` etc.).
///
/// Replaces the prior `compile_bindings_with_libs_excluding`
/// function — the "_excluding" suffix referred to a now-defunct
/// `exclude` parameter, and the function carried a legacy
/// semicolon-chain Map-bindings branch that no shipped workload
/// shape exercises (workload params are always present, so the
/// `needs_scope` gate always picked the modern path).
// reason: cohesive compile entry point — each argument is an
// independent compile input (parent scope, ops, lib paths, strictness,
// required-names, context label, cursor bound); bundling them into a
// struct would only move the same fields without adding clarity.
#[allow(clippy::too_many_arguments)]
pub fn build_workload_root_kernel(
    parent: &PolydatKernel,
    ops: &[ParsedOp],
    source_dir: Option<&std::path::Path>,
    polydat_lib_paths: Vec<std::path::PathBuf>,
    strict: bool,
    extra_required: &[String],
    context: &str,
    cursor_limit: Option<u64>,
    workload_params: &std::collections::HashMap<String, String>,
    workload_level_polydat: Option<&str>,
) -> Result<PolydatKernel, String> {
    // Build the workload-root scope. workload_params get
    // injected as `const` bindings so descendants resolve them
    // via the standard manifest auto-extern.
    let mut scope = crate::scope::build_scope(
        ops,
        &std::collections::HashMap::new(), // no iteration vars at outer level
        &[],                               // no outer manifest (this IS the outer scope)
        workload_params,
        &std::collections::HashMap::new(), // phases not needed here
        None,                              // no phase cycles
        &[],                               // no excluded names
        None,                              // workload-root has no parent program for AST mode
    )?;

    // SRD-13f §"Wire-reference classification" — the workload's
    // `bindings:` block lives on the workload-root scope as
    // local matter. Ingest before validate/emit so emit()
    // produces a single coherent source.
    if let Some(extra) = workload_level_polydat
        && !extra.trim().is_empty()
    {
        scope.ingest_polydat_source(extra, crate::scope::BindingOrigin::Inherited);
    }
    scope.validate().map_err(|e| format!("{context}: {e}"))?;

    // DCE-keepalive list: caller's config refs plus every
    // workload param. Param names are sorted so the resulting
    // output order is deterministic across processes (HashMap
    // iteration is randomised per-process, and `compile_filtered`
    // calls `add_output` in iteration order — non-determinism
    // here would surface as a non-deterministic
    // `canonical_hash` and break checkpoint resume-skip
    // matching).
    let mut scope_required = scope.required_outputs();
    for name in extra_required {
        if !scope_required.contains(name) {
            scope_required.push(name.clone());
        }
    }
    let mut param_names: Vec<&String> = workload_params.keys().collect();
    param_names.sort();
    for name in param_names {
        if !scope_required.contains(name) {
            scope_required.push(name.clone());
        }
    }

    // Standard ScopeKernel construction: PolydatMatter::source +
    // parent.build_subscope. Identical to every other scope's
    // build pathway (SRD-67 §"The construction protocol").
    let mut source = scope.emit();
    // If the workload's authored matter doesn't declare its
    // own `input ...: u64` line, default the workload-root
    // kernel's coordinate input to `cycle`. The wire name is
    // a CONVENTION the runner-driver contract relies on
    // (every dispatch path sets the cycle ordinal on slot 0
    // via set_inputs); workload authors who need a different
    // shape can declare their own inputs line explicitly. The
    // default lets inline workloads (`nmbrs run op="c={cycle}"`)
    // resolve `{cycle}` against the workload-root kernel
    // without forcing every author to write `input cycle: u64`.
    if !source.lines().any(|l| l.trim_start().starts_with("input ")) {
        source = format!("input cycle: u64\n{source}");
    }
    let opts = polydat::kernel::subcontext::CompileOptions {
        workload_dir: source_dir.map(|p| p.to_path_buf()),
        polydat_lib_paths,
        strict,
        required_outputs: scope_required,
        context_label: Some(context.to_string()),
        cursor_limit,
        ..Default::default()
    };
    // Mark workload-param names as inherited so their input slots
    // don't show up in `compute_own_coordinates` (which filters for
    // `IterationExtern`-kind inputs that aren't marked inherited).
    // Without this, switching the workload-param cascade from
    // `const NAME := <literal>` to `extern NAME: T` causes every
    // workload param to show up as a scope-coordinate on the
    // workload-root kernel — and downstream code (phase identity
    // construction, label formatting, scene-tree population) treats
    // those coordinates as iter-vars belonging to this scope. The
    // resulting phase-identity drift between pre-map registration
    // and runtime phase_failed silently breaks checkpoint Failed
    // recording. Marking them inherited keeps the cascade-pass-
    // through semantics: descendants still extern these names from
    // the workload-root's manifest, but the workload-root itself
    // treats them as inherited rather than as its own iter-vars.
    let mut inherited_param_names: Vec<String> = workload_params.keys().cloned().collect();
    inherited_param_names.sort();
    let matter = polydat::kernel::subcontext::PolydatMatter::builder()
        .label(context)
        .source(source)
        .inherited_outputs(inherited_param_names)
        .options(opts)
        .build()
        .map_err(|e| format!("{e:?}"))?;
    parent.build_subscope(matter).map_err(|e| format!("{e:?}"))
}

/// Compile all bindings from a set of ParsedOps into a Polydat Kernel.
///
/// When `strict` is true, the Polydat compiler enforces:
/// - Explicit `input ...: u64` declaration (no inference)
/// - All module arguments must be named (no positional)
/// - All module inputs must be provided by caller (no fallthrough)
pub fn compile_bindings_with_opts(
    ops: &[ParsedOp],
    source_dir: Option<&std::path::Path>,
    strict: bool,
) -> Result<PolydatKernel, String> {
    use nmbrs_workload::model::BindingsDef;

    // Check if any op uses Polydat source mode
    let polydat_source = ops.iter().find_map(|op| {
        if let BindingsDef::PolydatSource(src) = &op.bindings {
            if !src.trim().is_empty() {
                Some(src.clone())
            } else {
                None
            }
        } else {
            None
        }
    });

    if let Some(source) = polydat_source {
        // Native Polydat grammar mode: collect referenced bind points from
        // op templates for dead code elimination. Only the bindings
        // actually used by ops are compiled into the kernel.
        let mut required: Vec<String> = Vec::new();
        for op in ops {
            for value in op.op.values() {
                if let Some(s) = value.as_str() {
                    for name in bindpoints::referenced_bindings(s) {
                        if !required.contains(&name) {
                            required.push(name);
                        }
                    }
                }
            }
        }
        let options = polydat::dsl::compile::CompileOptions {
            source_dir: source_dir.map(std::path::Path::to_path_buf),
            required_outputs: required,
            strict,
            ..Default::default()
        };
        return polydat::dsl::compile::compile_polydat_with_options(&source, &options, None);
    }

    // Legacy mode: translate semicolon-chain bindings into Polydat source
    // and compile through the unified Polydat compiler. No separate dispatch
    // table — every node function available in Polydat grammar is automatically
    // available in legacy chain syntax.
    let mut all_bindings: HashMap<String, String> = HashMap::new();
    for op in ops {
        if let BindingsDef::Map(map) = &op.bindings {
            for (name, expr) in map {
                all_bindings
                    .entry(name.clone())
                    .or_insert_with(|| expr.clone());
            }
        }
    }

    // Collect required outputs from op templates
    let mut required: Vec<String> = Vec::new();
    for op in ops {
        for value in op.op.values() {
            if let Some(s) = value.as_str() {
                for name in bindpoints::referenced_bindings(s) {
                    if !required.contains(&name) {
                        required.push(name);
                    }
                }
            }
        }
    }

    // Translate each legacy chain into Polydat source lines
    let mut polydat_lines: Vec<String> = Vec::new();
    polydat_lines.push("input cycle: u64".into());

    for (binding_name, expr) in &all_bindings {
        let chain = parse_binding_chain(expr);
        if chain.is_empty() {
            return Err(format!("empty binding expression for '{binding_name}'"));
        }

        // Convert each chain step into a Polydat binding.
        // Chain: FuncA(args); FuncB(args) → sequential wiring from cycle.
        let mut prev_wire = "cycle".to_string();

        for (i, func) in chain.iter().enumerate() {
            let is_last = i == chain.len() - 1;
            let target = if is_last {
                binding_name.clone()
            } else {
                format!("__chain_{binding_name}_{i}")
            };

            // Translate legacy function names to Polydat equivalents
            let (func_name, extra_args) = translate_legacy_func(&func.name, &func.args);
            let mut call_args = vec![prev_wire.clone()];
            for arg in &func.args {
                call_args.push(strip_java_long_suffix(arg.trim()).to_string());
            }
            call_args.extend(extra_args);

            polydat_lines.push(format!(
                "{target} := {func_name}({args})",
                args = call_args.join(", ")
            ));

            prev_wire = target;
        }
    }

    // Validate: all bind point references must have a binding or
    // be a declared coordinate (the Polydat compiler auto-exposes
    // coordinates as passthrough outputs, so they resolve without
    // a user-declared binding).
    let coord_names: HashSet<String> = ["cycle".to_string()].into_iter().collect();
    let mut missing: Vec<String> = Vec::new();
    for name in &required {
        if !all_bindings.contains_key(name) && !coord_names.contains(name) {
            missing.push(name.clone());
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "undeclared bind point references: {}. Add these to your bindings section.",
            missing.join(", ")
        ));
    }

    let polydat_source = polydat_lines.join("\n");
    let options = polydat::dsl::compile::CompileOptions {
        source_dir: source_dir.map(std::path::Path::to_path_buf),
        required_outputs: required,
        strict,
        ..Default::default()
    };
    polydat::dsl::compile::compile_polydat_with_options(&polydat_source, &options, None)
}

// ---------------------------------------------------------------------------
// Legacy function name translation (virtdata → GK)
//
// This is a compatibility overlay that maps Java nosqlbench function
// names to their nmbrs Polydat equivalents. It lives ONLY in the binding
// chain compiler — the Polydat registry and node implementations are not
// polluted with legacy names.
// ---------------------------------------------------------------------------

/// Translate a legacy Java nosqlbench function name to its Polydat equivalent.
///
/// Returns `(polydat_func_name, extra_args)` where extra_args are additional
/// arguments to append (e.g., for ToString → format_u64 which needs a radix).
fn translate_legacy_func(name: &str, args: &[String]) -> (String, Vec<String>) {
    match name.to_lowercase().as_str() {
        // Direct name mappings (same semantics, different naming convention)
        "hash" => ("hash".into(), vec![]),
        "identity" => ("identity".into(), vec![]),
        "add" => ("add".into(), vec![]),
        "mul" => ("mul".into(), vec![]),
        "div" => ("div".into(), vec![]),
        "mod" => ("mod".into(), vec![]),
        "clamp" => ("clamp".into(), vec![]),

        // String conversions
        "tostring" | "to_string" => ("format_u64".into(), vec!["10".into()]),
        "tohexstring" => ("format_u64".into(), vec!["16".into()]),
        "tooctalstring" => ("format_u64".into(), vec!["8".into()]),
        "tobinarystring" => ("format_u64".into(), vec!["2".into()]),

        // Distributions (Java names → Polydat equivalents)
        // Uniform(min, max) → hash_range(input, max-min) + add(min)
        // This is approximate — Java Uniform samples from a distribution;
        // Polydat hash_range does modular hash. Close enough for key distribution.
        "uniform" => {
            if args.len() >= 2 {
                // Uniform(min, max) → mod(hash(input), range) then add(min)
                // We approximate with hash_range which takes just max
                ("hash_range".into(), vec![])
            } else {
                ("hash_range".into(), vec![])
            }
        }

        // Zipf, Normal, etc. — map to icd_ variants
        "normal" | "gaussian" => ("icd_normal".into(), vec![]),
        "zipf" => ("dist_zipf".into(), vec![]),

        // Hash-based
        "hashrange" | "hash_range" => ("hash_range".into(), vec![]),
        "hashinterval" | "hash_interval" => ("hash_interval".into(), vec![]),

        // Number formatting
        "format" | "printf" => ("printf".into(), vec![]),
        "numbernamesto_string" | "numbernames" => ("number_to_words".into(), vec![]),

        // Shuffle / permutation
        "shuffle" => ("shuffle".into(), vec![]),

        // Long suffix stripping (Java allows 1000000000L)
        _ => {
            // Default: lowercase the name and hope the Polydat registry has it
            (name.to_lowercase(), vec![])
        }
    }
}

/// Strip Java long literal suffix (e.g., "1000000000L" → "1000000000")
fn strip_java_long_suffix(arg: &str) -> &str {
    arg.strip_suffix('L')
        .or_else(|| arg.strip_suffix('l'))
        .unwrap_or(arg)
}

/// SRD-13f Push D: translate a Map-form bindings block (legacy
/// semicolon-chain syntax, e.g. `{user_id: "Hash(); Mod(1000000)"}`)
/// into Polydat source lines. Returns one or more `name := func(args)`
/// lines per entry — multi-step chains expand to intermediate
/// `__chain_<name>_<i> := ...` wires.
///
/// Does NOT prepend `input cycle: u64`; that's owned by the
/// enclosing scope's emit. Intended for routing workload-level
/// `bindings:` directly to the workload-root kernel without
/// going through the parser merge (which retired in Push D).
pub fn legacy_chain_map_to_polydat_lines(
    map: &std::collections::HashMap<String, String>,
) -> Result<String, String> {
    let mut polydat_lines: Vec<String> = Vec::new();
    for (binding_name, expr) in map {
        let chain = parse_binding_chain(expr);
        if chain.is_empty() {
            return Err(format!("empty binding expression for '{binding_name}'"));
        }
        let mut prev_wire = "cycle".to_string();
        for (i, func) in chain.iter().enumerate() {
            let is_last = i == chain.len() - 1;
            let target = if is_last {
                binding_name.clone()
            } else {
                format!("__chain_{binding_name}_{i}")
            };
            let (func_name, extra_args) = translate_legacy_func(&func.name, &func.args);
            let mut call_args = vec![prev_wire.clone()];
            for arg in &func.args {
                call_args.push(strip_java_long_suffix(arg.trim()).to_string());
            }
            call_args.extend(extra_args);
            polydat_lines.push(format!(
                "{target} := {func_name}({args})",
                args = call_args.join(", ")
            ));
            prev_wire = target;
        }
    }
    Ok(polydat_lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use polydat::dsl::pragmas::{Pragma, PragmaSet};

    #[test]
    fn prepend_pragmas_strict_alias() {
        let pragmas = PragmaSet {
            entries: vec![Pragma {
                name: "strict".into(),
                args: vec![],
                line: 1,
            }],
            parent: None,
        };
        let body = "id := cycle\n";
        let out = prepend_effective_pragmas(&pragmas, body);
        // `strict` activates both — emit single combined directive.
        assert!(out.starts_with("pragma strict\n"));
        assert!(out.contains("id := cycle"));
    }

    #[test]
    fn prepend_pragmas_individual_modes() {
        let pragmas = PragmaSet {
            entries: vec![Pragma {
                name: "strict_values".into(),
                args: vec![],
                line: 1,
            }],
            parent: None,
        };
        let out = prepend_effective_pragmas(&pragmas, "x := cycle");
        assert!(out.starts_with("pragma strict_values\n"));
        assert!(!out.contains("strict_types"));
    }

    #[test]
    fn prepend_pragmas_no_op_when_empty() {
        let pragmas = PragmaSet::default();
        let out = prepend_effective_pragmas(&pragmas, "x := cycle");
        assert_eq!(out, "x := cycle");
    }

    #[test]
    fn prepend_pragmas_walks_parent_chain() {
        // Parent declares strict_values. Child declares nothing.
        // The child's effective state (via chain walk) still
        // produces the prepended directive — that's the
        // load-bearing behavior for SRD 18b cross-scope
        // propagation.
        let parent = std::sync::Arc::new(PragmaSet {
            entries: vec![Pragma {
                name: "strict_values".into(),
                args: vec![],
                line: 1,
            }],
            parent: None,
        });
        let child = PragmaSet {
            entries: vec![],
            parent: Some(parent),
        };
        let out = prepend_effective_pragmas(&child, "x := cycle");
        assert!(
            out.starts_with("pragma strict_values\n"),
            "expected pragma to flow from parent chain, got:\n{out}"
        );
    }

    #[test]
    fn parse_simple_chain() {
        let chain = parse_binding_chain("Hash(); Mod(1000000)");
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].name, "Hash");
        assert!(chain[0].args.is_empty());
        assert_eq!(chain[1].name, "Mod");
        assert_eq!(chain[1].args, vec!["1000000"]);
    }

    #[test]
    fn parse_identity() {
        let chain = parse_binding_chain("Identity()");
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].name, "Identity");
    }

    #[test]
    fn parse_with_string_arg() {
        let chain = parse_binding_chain("Template('user-{}', ToString())");
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].name, "Template");
        assert_eq!(chain[0].args.len(), 2);
    }

    #[test]
    fn parse_long_chain() {
        let chain = parse_binding_chain("Add(10); Hash(); Mod(100); ToString()");
        assert_eq!(chain.len(), 4);
        assert_eq!(chain[0].name, "Add");
        assert_eq!(chain[1].name, "Hash");
        assert_eq!(chain[2].name, "Mod");
        assert_eq!(chain[3].name, "ToString");
    }

    #[test]
    fn parse_with_long_suffix() {
        let chain = parse_binding_chain("Mod(1000000000L)");
        assert_eq!(chain[0].args, vec!["1000000000L"]);
        // The L suffix is preserved in the chain parse.
        // The Polydat compiler handles it during node construction.
    }

    #[test]
    fn compile_identity_binding() {
        let ops = vec![{
            let mut op = ParsedOp::simple("test", "{myval}");
            op.bindings.insert("myval".into(), "Identity()".into());
            op
        }];
        let mut kernel = compile_bindings(&ops).unwrap();
        kernel.set_inputs(&[42]);
        assert_eq!(kernel.pull("myval").as_u64(), 42);
    }

    #[test]
    fn compile_hash_mod_binding() {
        let ops = vec![{
            let mut op = ParsedOp::simple("test", "{id}");
            op.bindings
                .insert("id".into(), "Hash(); Mod(1000000)".into());
            op
        }];
        let mut kernel = compile_bindings(&ops).unwrap();
        kernel.set_inputs(&[42]);
        let val = kernel.pull("id").as_u64();
        assert!(val < 1_000_000, "got {val}");
    }

    #[test]
    fn compile_hash_mod_deterministic() {
        let ops = vec![{
            let mut op = ParsedOp::simple("test", "{id}");
            op.bindings.insert("id".into(), "Hash(); Mod(100)".into());
            op
        }];
        let mut kernel = compile_bindings(&ops).unwrap();
        kernel.set_inputs(&[42]);
        let v1 = kernel.pull("id").as_u64();
        kernel.set_inputs(&[42]);
        let v2 = kernel.pull("id").as_u64();
        assert_eq!(v1, v2);
    }

    #[test]
    fn compile_multiple_bindings() {
        let ops = vec![{
            let mut op = ParsedOp::simple("test", "{a} {b}");
            op.bindings.insert("a".into(), "Identity()".into());
            op.bindings.insert("b".into(), "Hash(); Mod(100)".into());
            op
        }];
        let mut kernel = compile_bindings(&ops).unwrap();
        kernel.set_inputs(&[5]);
        assert_eq!(kernel.pull("a").as_u64(), 5);
        assert!(kernel.pull("b").as_u64() < 100);
    }

    #[test]
    fn compile_rejects_undeclared_bind_points() {
        // Op references {mystery} but no binding declared → error
        let ops = vec![ParsedOp::simple("test", "val={mystery}")];
        let result = compile_bindings(&ops);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("undeclared bind point"));
    }

    #[test]
    fn compile_add_chain() {
        let ops = vec![{
            let mut op = ParsedOp::simple("test", "{val}");
            op.bindings
                .insert("val".into(), "Add(100); Mod(1000)".into());
            op
        }];
        let mut kernel = compile_bindings(&ops).unwrap();
        kernel.set_inputs(&[5]);
        // 5 + 100 = 105, 105 % 1000 = 105
        assert_eq!(kernel.pull("val").as_u64(), 105);
    }

    #[test]
    fn compile_provides_cycle_output() {
        let ops = vec![ParsedOp::simple("test", "cycle={cycle}")];
        let mut kernel = compile_bindings(&ops).unwrap();
        kernel.set_inputs(&[99]);
        assert_eq!(kernel.pull("cycle").as_u64(), 99);
    }

    #[test]
    fn legacy_tostring_translates() {
        let (name, _) = translate_legacy_func("ToString", &[]);
        assert_eq!(name, "format_u64");
    }

    #[test]
    fn legacy_uniform_translates() {
        let (name, _) = translate_legacy_func("Uniform", &["0".into(), "1000".into()]);
        assert_eq!(name, "hash_range");
    }
}
