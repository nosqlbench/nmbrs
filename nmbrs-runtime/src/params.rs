// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Workload parameters as a Polydat module.
//!
//! Workload params arrive as `(name → string)` pairs from
//! the YAML `params:` block plus any CLI overrides. Rather
//! than text-substituting their values into op bindings or
//! patching them as folded constants on every kernel that
//! happens to need them, we compile them once into a
//! standalone Polydat Kernel — the **workload-params kernel** —
//! which sits at the root of the scope chain. Every kernel
//! built downstream (workload-level bindings, phase ops,
//! comprehensions, leaf phases) `materialize_wiring_from_outer`s through
//! it, so `{name}` references in any descendant resolve via
//! standard Polydat name resolution.
//!
//! ## Why a kernel instead of a string-substitution pass
//!
//! Text replacement of `{name}` placeholders into op binding
//! sources is fundamentally ambiguous: a placeholder can sit
//! inside a string literal (`"{dataset}:{profile}"`) where
//! it's Polydat string-interpolation, or as a standalone expression
//! where it's an identifier reference. A blind text pass
//! rewrites both, double-quotes the string-literal cases, and
//! produces broken Polydat source.
//!
//! The params-kernel approach is unambiguous — `final name :=
//! <literal>` is just a normal Polydat binding. Polydat's parser knows
//! string-interpolation from identifier reference; both
//! resolve correctly.
//!
//! ## Type detection
//!
//! Since workload params arrive as strings, we infer Polydat types
//! the same way the legacy [`crate::scope::format_workload_param_as_polydat_literal`]
//! does:
//!
//! - Integer-parseable → `u64`
//! - Float-parseable → `f64`
//! - `"true"` / `"false"` → `bool`
//! - Anything else → `String` (with proper quote / escape handling)
//!
//! Native typing matters because descendant-scope synthesis
//! relies on the parent's manifest port types when emitting
//! cascade externs (see SRD-18b §"Cascade externs"). A param
//! presented as `"100"` becomes `u64` in the manifest, not
//! `String`.

use std::collections::HashMap;

use polydat::dsl::compile::compile_polydat;
use polydat::kernel::PolydatKernel;

/// Build the workload-params kernel from a params map. The
/// resulting kernel exposes one `final <name> := <literal>`
/// binding per param; consumers `materialize_wiring_from_outer` to it to
/// inherit every workload param at once.
///
/// Empty params produces a kernel with a single
/// `const __empty := 0` placeholder so descendant scopes can
/// always `materialize_wiring_from_outer` to it without a "no kernel"
/// special case.
pub fn build_workload_params_kernel(
    params: &HashMap<String, String>,
) -> Result<PolydatKernel, String> {
    let source = render_workload_params_source(params);
    compile_polydat(&source)
        .map_err(|e| format!("workload params kernel: {e}\n--- generated source ---\n{source}"))
}

/// Render the Polydat source for the workload-params kernel. Public
/// so tests and diagnostics can inspect the synthesized module
/// without compiling it.
pub fn render_workload_params_source(params: &HashMap<String, String>) -> String {
    if params.is_empty() {
        return "const __empty := 0\n".to_string();
    }
    // Sort by name so the generated source is deterministic
    // across runs — matters for cache keys, diagnostic output,
    // and golden-output tests.
    let mut keys: Vec<&String> = params.keys().collect();
    keys.sort();
    let mut out = String::new();
    for name in keys {
        let value = &params[name];
        let literal = format_value_as_polydat_literal(value);
        out.push_str(&format!("const {name} := {literal}\n"));
    }
    out
}

/// Format a workload-ROOT-param string as a Polydat literal.
///
/// Workload-root params have NO outer scope to reference, so
/// bare identifier-shaped tokens lower to polydat STRING
/// LITERALS (the legacy behavior) rather than wire references.
/// The new array / quoted-string / numeric paths still apply.
///
/// `set:` block bindings (in `nmbrs-workload/src/parse.rs`) AND
/// per-scope param bindings (in `scope::add_param_binding`)
/// use a DIFFERENT classifier — at those sites the operator
/// CAN intend a wire reference because an outer scope exists.
///
/// Surface here:
///
/// - Bare U64 / F64 / Bool literals → emit as-is
/// - Polydat-quoted string `"…"` → emit as-is (the YAML carrier
///   was `'"value"'`; the polydat-syntax quotes survived)
/// - Polydat array literal `[…]` → emit as-is (a YAML array
///   value flattened through `format_jval_as_polydat_literal`)
/// - Anything else (including bare-identifier-shaped tokens) →
///   wrap as Polydat string literal, escaping `\` and `"`
fn format_value_as_polydat_literal(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.parse::<u64>().is_ok() || trimmed.parse::<f64>().is_ok() {
        return trimmed.to_string();
    }
    if is_polydat_quoted_string(trimmed) || is_polydat_array_literal(trimmed) {
        return trimmed.to_string();
    }
    // Bare `true` / `false` AND anything else fall through to
    // quoted-string. Polydat's lexer has no boolean token kind,
    // so a workload param value of `flag_t: true` lowers to
    // `const flag_t := "true"` (a Str) — downstream consumers
    // can `str_eq(flag_t, "true")` if they need a boolean
    // comparison. This is the legacy workload-root behavior;
    // the `set:` block parser (which IS in a polydat-aware
    // scope context) handles bool literals differently.
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn is_polydat_quoted_string(s: &str) -> bool {
    if s.len() < 2 {
        return false;
    }
    if !s.starts_with('"') || !s.ends_with('"') {
        return false;
    }
    let bytes = s.as_bytes();
    let mut i = 1;
    let last = bytes.len() - 1;
    while i < last {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == b'"' {
            return false;
        }
        i += 1;
    }
    true
}

fn is_polydat_array_literal(s: &str) -> bool {
    if !s.starts_with('[') || !s.ends_with(']') {
        return false;
    }
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    for c in s.chars() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn renders_empty_params_as_placeholder() {
        let src = render_workload_params_source(&HashMap::new());
        assert_eq!(src, "const __empty := 0\n");
    }

    #[test]
    fn renders_typed_params_with_native_literals() {
        let src = render_workload_params_source(&h(&[
            ("dataset", "example"),
            ("k_values", "1,10"),
            ("count", "100"),
            ("ratio", "0.95"),
            ("strict", "true"),
        ]));
        // Sorted by name: count, dataset, k_values, ratio, strict.
        // Numbers pass through as bare literals; booleans round-
        // trip as quoted strings because Polydat has no bool token kind
        // (a bare `true` would parse as an identifier).
        let expected = "const count := 100\n\
                        const dataset := \"example\"\n\
                        const k_values := \"1,10\"\n\
                        const ratio := 0.95\n\
                        const strict := \"true\"\n";
        assert_eq!(src, expected);
    }

    #[test]
    fn boolean_strings_compile_as_string_consts() {
        // Regression: workload params like
        //   enable_hierarchy: "false"
        // used to emit `const enable_hierarchy := false`, which
        // Polydat rejected as an unknown wire. They must round-trip as
        // quoted strings so the params-kernel compiles.
        let kernel = build_workload_params_kernel(&h(&[
            ("enable_hierarchy", "false"),
            ("debug_mode", "true"),
        ]))
        .unwrap();
        let eh = kernel
            .lookup("enable_hierarchy")
            .expect("enable_hierarchy must resolve");
        let dm = kernel
            .lookup("debug_mode")
            .expect("debug_mode must resolve");
        assert_eq!(eh.to_display_string(), "false");
        assert_eq!(dm.to_display_string(), "true");
    }

    #[test]
    fn escapes_quotes_in_string_values() {
        let src = render_workload_params_source(&h(&[
            ("replication", r#"{'class': 'SimpleStrategy'}"#),
            ("with_quote", r#"a"b"#),
        ]));
        assert!(
            src.contains(r#"const replication := "{'class': 'SimpleStrategy'}""#),
            "unexpected: {src}"
        );
        assert!(
            src.contains(r#"const with_quote := "a\"b""#),
            "embedded double-quote not escaped: {src}"
        );
    }

    #[test]
    fn deterministic_ordering_across_runs() {
        let p = h(&[("z_last", "1"), ("a_first", "2"), ("m_middle", "3")]);
        let s1 = render_workload_params_source(&p);
        let s2 = render_workload_params_source(&p);
        assert_eq!(s1, s2);
        // Names appear alphabetically.
        let a_pos = s1.find("a_first").unwrap();
        let m_pos = s1.find("m_middle").unwrap();
        let z_pos = s1.find("z_last").unwrap();
        assert!(a_pos < m_pos && m_pos < z_pos);
    }

    #[test]
    fn compiles_to_valid_kernel() {
        let kernel =
            build_workload_params_kernel(&h(&[("dataset", "example"), ("count", "100")])).unwrap();
        // Both params are reachable as Polydat constants.
        let dataset = kernel.lookup("dataset").expect("dataset must resolve");
        let count = kernel.lookup("count").expect("count must resolve");
        assert_eq!(dataset.to_display_string(), "example");
        assert_eq!(count.as_u64(), 100);
    }

    #[test]
    fn compiles_with_no_params_using_placeholder() {
        let kernel = build_workload_params_kernel(&HashMap::new()).unwrap();
        // `__empty` is folded — kernel compiles and is
        // materialize_wiring_from_outer-eligible. We don't assert the
        // placeholder is queryable since callers should
        // ignore it.
        let _ = kernel;
    }

    #[test]
    fn boolean_values_emit_quoted_strings() {
        // Polydat's lexer has no boolean token kind, so bare `true` /
        // `false` would parse as identifiers (wire references)
        // and fail kernel compilation. The formatter therefore
        // emits boolean-looking strings as quoted string
        // literals; downstream consumers that need a real
        // boolean comparison can `str_eq(x, "true")`.
        let src = render_workload_params_source(&h(&[("flag_t", "true"), ("flag_f", "false")]));
        assert!(src.contains("const flag_f := \"false\"\n"));
        assert!(src.contains("const flag_t := \"true\"\n"));
    }
}
