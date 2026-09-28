// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Surface #7 helpers — pure utilities for translating between
//! typed runtime values and Polydat source-text form.
//!
//! These functions encode nmbrs-runtime's Polydat source conventions:
//! - how a [`polydat::ast::Value`] becomes a fold-eligible GK
//!   literal,
//! - how a workload-param string becomes a quoted-or-numeric GK
//!   literal,
//! - how a [`polydat::ast::PortType`] becomes an `extern`
//!   declaration type name,
//! - how `{name}` placeholders are scanned out of clause / body
//!   text.
//!
//! They're pure functions with no comprehension- or scope-
//! specific knowledge; the scope builders import them as walking
//! primitives. The polydat side retains parallel copies during
//! the cutover (still used by `polydat::iteration::comprehension::synthesize_for_each_scope`);
//! those copies retire when the legacy synthesis module is
//! deleted at the end of PR 9c-1b.

use std::collections::HashSet;

use polydat::ast::{PortType, Value};

/// Pick the Polydat port type for a workload-param string value.
///
/// Numeric values widen to `u64` / `f64`; `true`/`false` →
/// `bool`; everything else → `String`.
pub fn workload_param_type_name(value: &str) -> &'static str {
    let trimmed = value.trim();
    if trimmed.parse::<u64>().is_ok() {
        "u64"
    } else if trimmed.parse::<f64>().is_ok() {
        "f64"
    } else if trimmed == "true" || trimmed == "false" {
        "bool"
    } else {
        "String"
    }
}

/// Format a typed [`Value`] as a Polydat source literal — strict
/// variant. Returns `None` when the value isn't representable as
/// a literal (`Bytes`, `Json`, `Ext`, `Handle`, vectors). Used
/// by the for_each scope synthesizer when inlining const-folded
/// parent outputs; falls back to extern cascade in `None` cases.
pub fn format_value_as_final_literal(v: &Value) -> Option<String> {
    match v {
        Value::U64(n) => Some(n.to_string()),
        Value::F64(f) => {
            if f.fract() == 0.0 && f.is_finite() {
                Some(format!("{f:.1}"))
            } else {
                Some(format!("{f}"))
            }
        }
        Value::Bool(b) => Some(b.to_string()),
        Value::Str(s) => {
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            Some(format!("\"{escaped}\""))
        }
        _ => None,
    }
}

/// Format a typed [`Value`] as a Polydat source literal.
///
/// Used when emitting `final <name> := <literal>` lines for
/// per-iteration scope synthesis. Falls back to a quoted-display
/// form for non-scalar variants (acceptable for iter-vars, which
/// are scalar in practice).
pub fn format_value_as_polydat_literal(v: &Value) -> String {
    match v {
        Value::U64(n) => n.to_string(),
        Value::F64(f) => {
            // Always include decimal point so the parser sees an
            // f64 literal, not an integer.
            if f.fract() == 0.0 && f.is_finite() {
                format!("{f:.1}")
            } else {
                format!("{f}")
            }
        }
        Value::Bool(b) => b.to_string(),
        Value::Str(s) => {
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        }
        _ => {
            let display = v.to_display_string();
            let escaped = display.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        }
    }
}

/// Format a workload-param string value as a Polydat literal.
///
/// Numeric inputs pass through untouched; non-numeric inputs get
/// wrapped in double quotes (with `\` / `"` escaping). The lexer
/// has no boolean token kind, so a bare `false` would parse as
/// an identifier (wire reference) and break kernel compilation;
/// this routes through the string path.
pub fn format_workload_param_as_polydat_literal(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.parse::<u64>().is_ok() || trimmed.parse::<f64>().is_ok() {
        trimmed.to_string()
    } else {
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    }
}

/// Convert a scalar [`Value`] to its workload-param string form.
///
/// Returns `None` for non-scalar variants (vectors, JSON,
/// handles, bytes, ext) — those aren't representable as GK
/// source literals.
pub fn value_to_param_string(v: &Value) -> Option<String> {
    match v {
        Value::U64(n) => Some(n.to_string()),
        Value::F64(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Str(s) => Some(s.to_string()),
        _ => None,
    }
}

/// Map a Polydat [`PortType`] to the extern declaration's type
/// keyword. Thin wrapper over [`PortType::to_keyword`] — the
/// canonical str↔PortType table on the enum itself — so every
/// synthesized `extern <name>: <keyword>` round-trips byte-cleanly
/// back through the DSL parser.
pub fn port_type_to_extern_name(t: PortType) -> &'static str {
    t.to_keyword()
}

/// Collect every leaf `{name}` placeholder from a list of clause
/// spec texts.
///
/// "Leaf" means a `{...}` whose body contains no further `{` —
/// the dynamic case (`{a_{b}_c}`) is handled at runtime by the
/// iterative interpolator. Honors `\{` / `\}` escapes (same
/// escape syntax `interpolate` uses).
pub fn collect_leaf_placeholders(texts: &[String]) -> HashSet<String> {
    let mut out = HashSet::new();
    for text in texts {
        scan_one(text, &mut out);
    }
    out
}

/// Scan one text for leaf `{name}` placeholders, inserting each
/// into `out`. Companion to [`collect_leaf_placeholders`] for
/// callers that already have a [`HashSet`] to grow incrementally.
pub fn scan_one(text: &str, out: &mut HashSet<String>) {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        if bytes[i] == b'\\' && i + 1 < n && (bytes[i + 1] == b'{' || bytes[i + 1] == b'}') {
            i += 2;
            continue;
        }
        if bytes[i] == b'{' {
            let mut j = i + 1;
            let mut nested = false;
            while j < n {
                if bytes[j] == b'\\' && j + 1 < n && (bytes[j + 1] == b'{' || bytes[j + 1] == b'}')
                {
                    j += 2;
                    continue;
                }
                if bytes[j] == b'{' {
                    nested = true;
                    break;
                }
                if bytes[j] == b'}' {
                    break;
                }
                j += 1;
            }
            if !nested && j < n && bytes[j] == b'}' {
                let name = &text[i + 1..j];
                if !name.is_empty() {
                    out.insert(name.to_string());
                }
                i = j + 1;
                continue;
            }
            i += 1;
            continue;
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workload_param_type_name_classifies_basic_scalars() {
        assert_eq!(workload_param_type_name("42"), "u64");
        assert_eq!(workload_param_type_name("3.14"), "f64");
        assert_eq!(workload_param_type_name("true"), "bool");
        assert_eq!(workload_param_type_name("false"), "bool");
        assert_eq!(workload_param_type_name("hello"), "String");
    }

    #[test]
    fn format_value_as_polydat_literal_renders_scalars() {
        assert_eq!(format_value_as_polydat_literal(&Value::U64(42)), "42");
        assert_eq!(format_value_as_polydat_literal(&Value::Bool(true)), "true");
        assert_eq!(
            format_value_as_polydat_literal(&Value::Str("x".into())),
            "\"x\""
        );
        // f64 with integral value still gets decimal point so the
        // parser doesn't see it as u64.
        assert_eq!(format_value_as_polydat_literal(&Value::F64(2.0)), "2.0");
    }

    #[test]
    fn format_workload_param_quotes_non_numeric() {
        assert_eq!(format_workload_param_as_polydat_literal("42"), "42");
        assert_eq!(
            format_workload_param_as_polydat_literal("hello"),
            "\"hello\""
        );
        // Bool string routes through quoted path (lexer has no
        // bool keyword).
        assert_eq!(format_workload_param_as_polydat_literal("true"), "\"true\"");
    }

    #[test]
    fn value_to_param_string_handles_scalars_only() {
        assert_eq!(value_to_param_string(&Value::U64(7)), Some("7".to_string()));
        assert_eq!(
            value_to_param_string(&Value::Str("y".into())),
            Some("y".to_string())
        );
        // Non-scalar variants return None — caller falls back.
        assert_eq!(value_to_param_string(&Value::None), None);
    }

    #[test]
    fn collect_leaf_placeholders_extracts_simple_names() {
        let names = collect_leaf_placeholders(&[
            "k in 1..{n}".to_string(),
            "{profile} matches {prefix}".to_string(),
        ]);
        assert!(names.contains("n"));
        assert!(names.contains("profile"));
        assert!(names.contains("prefix"));
    }

    #[test]
    fn collect_leaf_placeholders_skips_nested() {
        // Dynamic `{a_{b}_c}` form — outer placeholder skipped
        // (resolved at runtime), inner is extracted.
        let names = collect_leaf_placeholders(&["{a_{b}_c}".to_string()]);
        assert!(names.contains("b"));
        assert!(!names.contains("a_{b}_c"));
    }

    #[test]
    fn collect_leaf_placeholders_honors_escapes() {
        let names = collect_leaf_placeholders(&["\\{not_a_placeholder\\}".to_string()]);
        assert!(names.is_empty());
    }
}
