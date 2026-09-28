// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Inline workload synthesis from the `op=` command-line parameter.
//!
//! Parses an inline op template string into a [`Workload`] — the same
//! type that [`parse_workload()`](crate::parse::parse_workload) returns
//! from YAML. Inline `{{expr}}` bindings are extracted, assigned
//! synthetic Polydat output names, and compiled to a Polydat source block.
//!
//! See SRD 35 for design details.

use std::collections::HashMap;

use crate::model::{BindingsDef, ParsedOp, Workload};

/// Synthesize a [`Workload`] from an inline `op=` string.
///
/// # Inline binding syntax
///
/// - `{{expr}}` — inline Polydat expression. Compiled into the GK
///   kernel at init time, then invoked per cycle like any other
///   Polydat output. Extracted and replaced with `{__inline_N}`.
/// - `{name}` — reference bind point, resolved by the standard
///   bind point pipeline (GK output, coordinate, capture).
///
/// # Multiple ops
///
/// Semicolons separate multiple ops. An optional `N:` prefix sets
/// the ratio:
///
/// ```text
/// "3:read {{cycle}};1:write {{mod(cycle, 100)}}"
/// ```
///
/// # Examples
///
/// ```
/// use nmbrs_workload::inline::synthesize_inline_workload;
///
/// let w = synthesize_inline_workload("hello {{cycle}}").unwrap();
/// assert_eq!(w.ops.len(), 1);
/// assert_eq!(w.ops[0].name, "inline_0");
/// ```
pub fn synthesize_inline_workload(op_template: &str) -> Result<Workload, String> {
    if op_template.trim().is_empty() {
        return Err("op= value is empty".into());
    }

    // Polydat bindings-block form: `op='a := ...; b := ...'`. When the
    // whole value is a set of `name := expr` assignments that compiles
    // as valid Polydat, treat it as a bindings block whose binding
    // names become the op's fields — so any adapter consumes the named
    // outputs (stdout prints them, plotter plots them). Falls through
    // to the text-template form when it isn't valid Polydat.
    if let Some(w) = try_polydat_block_workload(op_template) {
        return Ok(w);
    }

    // Split on unquoted semicolons into individual op segments.
    let segments = split_ops(op_template);

    // Collect all inline expressions across all segments to build
    // a single shared Polydat source block.
    let mut inline_exprs: Vec<String> = Vec::new();
    let mut expr_index: HashMap<String, usize> = HashMap::new();

    // First pass: discover all inline expressions across all segments.
    // Sources: {{expr}} (double-brace), {expr} (detected expression),
    // {:=expr}, {:=expr:=}
    for seg in &segments {
        // Double-brace {{expr}}
        for expr in extract_inline_exprs(&seg.template) {
            if !expr_index.contains_key(&expr) {
                let idx = inline_exprs.len();
                expr_index.insert(expr.clone(), idx);
                inline_exprs.push(expr);
            }
        }
        // Single-brace expressions detected by bind point parser
        for bp in crate::bindpoints::extract_bind_points(&seg.template) {
            if let crate::bindpoints::BindPoint::InlineDefinition(expr) = bp
                && !expr_index.contains_key(&expr)
            {
                let idx = inline_exprs.len();
                expr_index.insert(expr.clone(), idx);
                inline_exprs.push(expr);
            }
        }
    }

    // Build Polydat source. The `input cycle: u64` line is always emitted
    // for inline mode: `cycle` is the wire name CLI users reference
    // (via `{cycle}` placeholders) without writing any explicit
    // bindings block, so the inline parser makes that convention
    // explicit in the generated model. Without this declaration the
    // workload-level placeholder validator can't see `cycle` as a
    // known wire name and rejects `{cycle}` as undeclared.
    let mut polydat_source = String::from("input cycle: u64\n");
    for (i, expr) in inline_exprs.iter().enumerate() {
        polydat_source.push_str(&format!("__inline_{i} := {expr}\n"));
    }

    // Second pass: rewrite templates and build ParsedOps.
    let mut ops = Vec::with_capacity(segments.len());
    for (i, seg) in segments.iter().enumerate() {
        let rewritten = rewrite_template(&seg.template, &expr_index);

        let mut op = ParsedOp::simple(&format!("inline_{i}"), &rewritten);

        if seg.ratio != 1 {
            op.params.insert(
                "ratio".to_string(),
                serde_json::Value::Number(serde_json::Number::from(seg.ratio)),
            );
        }

        op.tags.insert("name".to_string(), op.name.clone());
        op.tags.insert("op".to_string(), op.name.clone());
        op.tags.insert("block".to_string(), "inline".to_string());

        op.bindings = BindingsDef::PolydatSource(polydat_source.clone());

        ops.push(op);
    }

    Ok(Workload {
        description: Some("inline workload".into()),
        scenarios: HashMap::new(),
        stop_when: Vec::new(),
        ops,
        bindings: crate::model::BindingsDef::default(),
        params: HashMap::new(),
        phases: HashMap::new(),
        phase_order: Vec::new(),
        declared_params: Vec::new(),
        report: crate::report::Report::default(),
        report_warnings: Vec::new(),
        resolution_warnings: Vec::new(),
        scenario_parse_errors: Vec::new(),
        status_metrics: Vec::new(),
        readouts: crate::model::ReadoutsBindings::default(),
        wrappers: None,
        implements: None,
        stick_session: None,
    })
}

// ─── Polydat-block form ─────────────────────────────────────

/// Interpret `op=` as a Polydat program when it is one. The rule is
/// uniform — there are no special syntactic cases for `name := …`
/// vs `{…}` vs a bare expression: a candidate Polydat source is built
/// and handed to the compiler, and if it compiles, that's a Polydat
/// block. Its compiled OUTPUTS become the op's fields, so every
/// adapter consumes them (stdout prints them, plotter plots them).
/// Returns `None` when it doesn't compile, so the caller falls back
/// to the text-template form (which still resolves `{ref}` /
/// `{{expr}}` interpolation, i.e. the string-composite form).
fn try_polydat_block_workload(op_template: &str) -> Option<Workload> {
    let source = build_polydat_candidate(op_template);
    // The compiler is the sole arbiter of "is this Polydat?".
    polydat::dsl::compile::compile_polydat(&source).ok()?;
    // Fields are the declared wire names (`name := …`) — taken from the
    // source, since the compiler mangles *output* names with `__anon`
    // suffixes whereas the wires keep their declared names (what
    // `{name}` placeholders resolve against).
    let names = binding_wire_names(&source);
    if names.is_empty() {
        return None;
    }
    let mut op_fields: HashMap<String, serde_json::Value> = HashMap::new();
    for n in &names {
        // `{name}` resolves to wire `name` via the adapter's
        // `resolve_op_fields_via_wires`.
        op_fields.insert(n.clone(), serde_json::Value::String(format!("{{{n}}}")));
    }
    let mut op = ParsedOp::simple("inline_0", "");
    op.op = op_fields;
    op.bindings = BindingsDef::PolydatSource(source);
    op.tags.insert("name".to_string(), "inline_0".to_string());
    op.tags.insert("op".to_string(), "inline_0".to_string());
    op.tags.insert("block".to_string(), "inline".to_string());

    Some(Workload {
        description: Some("inline polydat workload".into()),
        scenarios: HashMap::new(),
        stop_when: Vec::new(),
        ops: vec![op],
        bindings: crate::model::BindingsDef::default(),
        params: HashMap::new(),
        phases: HashMap::new(),
        phase_order: Vec::new(),
        declared_params: Vec::new(),
        report: crate::report::Report::default(),
        report_warnings: Vec::new(),
        resolution_warnings: Vec::new(),
        scenario_parse_errors: Vec::new(),
        status_metrics: Vec::new(),
        readouts: crate::model::ReadoutsBindings::default(),
        wrappers: None,
        implements: None,
        stick_session: None,
    })
}

/// Build a candidate Polydat source from an `op=` spec: split on
/// top-level `;` into statements, declare `cycle`, and give any bare
/// trailing expression an `out :=` so the program has an output. An
/// already-complete `name := …` statement is kept verbatim. This is
/// pure source construction — whether the result is Polydat is decided
/// by the compiler, not by inspecting the shape here.
fn build_polydat_candidate(op_template: &str) -> String {
    let segs: Vec<String> = split_top_level_semicolons(op_template)
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let mut lines = vec!["input cycle: u64".to_string()];
    let last = segs.len().saturating_sub(1);
    for (i, seg) in segs.iter().enumerate() {
        if has_top_level_assignment(seg) {
            lines.push(seg.clone());
        } else if i == last {
            lines.push(format!("out := {seg}"));
        } else {
            lines.push(format!("__expr_{i} := {seg}"));
        }
    }
    lines.join("\n") + "\n"
}

/// Declared wire names from a built candidate source: the LHS
/// identifier of each `name := …` line (last whitespace token, so
/// `const x` → `x`), skipping the `input` line and internal
/// `__`-prefixed wraps.
pub(crate) fn binding_wire_names(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with("input ") || !line.contains(":=") {
                return None;
            }
            let lhs = line.split(":=").next()?.trim();
            let name = lhs.split_whitespace().last()?;
            let is_ident = !name.is_empty()
                && name.chars().all(|c| c.is_alphanumeric() || c == '_')
                && name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphabetic() || c == '_');
            if is_ident && !name.starts_with("__") {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// True when `s` contains a `:=` at brace-depth 0 (a real statement
/// boundary, not one buried inside `{{expr}}` / `{ref}`).
fn has_top_level_assignment(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => depth = (depth - 1).max(0),
            b':' if depth == 0 && i + 1 < bytes.len() && bytes[i + 1] == b'=' => {
                return true;
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// Split `s` on `;` at brace-depth 0.
fn split_top_level_semicolons(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '{' => {
                depth += 1;
                cur.push(c);
            }
            '}' => {
                depth = (depth - 1).max(0);
                cur.push(c);
            }
            ';' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

// ─── Internal Types ─────────────────────────────────────────

struct OpSegment {
    template: String,
    ratio: u64,
}

// ─── Helpers ────────────────────────────────────────────────

/// Split an op string on unquoted semicolons, extracting optional
/// ratio prefixes (`3:template`).
fn split_ops(input: &str) -> Vec<OpSegment> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut in_braces = 0u32;

    for c in input.chars() {
        match c {
            '{' => {
                in_braces += 1;
                current.push(c);
            }
            '}' => {
                in_braces = in_braces.saturating_sub(1);
                current.push(c);
            }
            ';' if in_braces == 0 => {
                let seg = current.trim().to_string();
                if !seg.is_empty() {
                    segments.push(parse_segment(&seg));
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    let seg = current.trim().to_string();
    if !seg.is_empty() {
        segments.push(parse_segment(&seg));
    }
    segments
}

/// Parse a single segment, extracting an optional `N:` ratio prefix.
fn parse_segment(s: &str) -> OpSegment {
    // Look for `N:` at the start, but don't confuse with `{{...}}`.
    if let Some(colon_pos) = s.find(':') {
        let prefix = &s[..colon_pos];
        // Only treat as ratio if prefix is all digits.
        if !prefix.is_empty()
            && prefix.chars().all(|c| c.is_ascii_digit())
            && let Ok(ratio) = prefix.parse::<u64>()
        {
            return OpSegment {
                template: s[colon_pos + 1..].trim().to_string(),
                ratio,
            };
        }
    }
    OpSegment {
        template: s.to_string(),
        ratio: 1,
    }
}

/// Extract all `{{expr}}` occurrences from a template string.
fn extract_inline_exprs(template: &str) -> Vec<String> {
    let mut exprs = Vec::new();
    let bytes = template.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i + 1 < len {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Find matching }}.
            let start = i + 2;
            let mut depth = 1u32;
            let mut j = start;
            while j + 1 < len {
                if bytes[j] == b'{' && bytes[j + 1] == b'{' {
                    depth += 1;
                    j += 2;
                } else if bytes[j] == b'}' && bytes[j + 1] == b'}' {
                    depth -= 1;
                    if depth == 0 {
                        let expr = template[start..j].trim().to_string();
                        if !expr.is_empty() {
                            exprs.push(expr);
                        }
                        i = j + 2;
                        break;
                    }
                    j += 2;
                } else {
                    j += 1;
                }
            }
            if depth > 0 {
                // Unmatched {{ — skip past it.
                i += 2;
            }
        } else {
            i += 1;
        }
    }
    exprs
}

/// Rewrite a template by replacing inline expressions with `{__inline_N}`.
/// Handles both `{{expr}}` (double-brace) and single-brace expressions
/// ({:=expr}, {:=expr:=}, and auto-detected {expr}).
fn rewrite_template(template: &str, expr_index: &HashMap<String, usize>) -> String {
    // First pass: rewrite {{expr}} double-brace forms
    let after_double = rewrite_double_brace(template, expr_index);
    // Second pass: rewrite single-brace expressions {expr}, {:=expr}, {:=expr:=}
    rewrite_single_brace_exprs(&after_double, expr_index)
}

fn rewrite_single_brace_exprs(template: &str, expr_index: &HashMap<String, usize>) -> String {
    let mut result = String::with_capacity(template.len());
    let chars: Vec<char> = template.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '{' && (i + 1 >= chars.len() || chars[i + 1] != '{') {
            let start = i + 1;
            let mut depth = 1u32;
            let mut j = start;
            while j < chars.len() {
                if chars[j] == '{' {
                    depth += 1;
                }
                if chars[j] == '}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                j += 1;
            }
            if j < chars.len() {
                let raw: String = chars[start..j].iter().collect();
                let raw = raw.trim();

                // Check for {:=expr} or {:=expr:=}
                let expr = if let Some(e) = raw.strip_prefix(":=") {
                    Some(e.strip_suffix(":=").unwrap_or(e).trim())
                } else if crate::bindpoints::is_expression_public(raw) {
                    Some(raw)
                } else {
                    None
                };

                if let Some(expr) = expr {
                    if let Some(&idx) = expr_index.get(expr) {
                        result.push_str(&format!("{{__inline_{idx}}}"));
                    } else {
                        // Not in index — preserve as-is
                        result.push('{');
                        result.push_str(raw);
                        result.push('}');
                    }
                } else {
                    // Simple reference — preserve
                    result.push('{');
                    result.push_str(raw);
                    result.push('}');
                }
                i = j + 1;
            } else {
                result.push(chars[i]);
                i += 1;
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

fn rewrite_double_brace(template: &str, expr_index: &HashMap<String, usize>) -> String {
    let mut result = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if i + 1 < len && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let start = i + 2;
            let mut depth = 1u32;
            let mut j = start;
            while j + 1 < len {
                if bytes[j] == b'{' && bytes[j + 1] == b'{' {
                    depth += 1;
                    j += 2;
                } else if bytes[j] == b'}' && bytes[j + 1] == b'}' {
                    depth -= 1;
                    if depth == 0 {
                        let expr = template[start..j].trim().to_string();
                        if let Some(&idx) = expr_index.get(&expr) {
                            result.push_str(&format!("{{__inline_{idx}}}"));
                        } else {
                            // Should not happen, but preserve original.
                            result.push_str(&template[i..j + 2]);
                        }
                        i = j + 2;
                        break;
                    }
                    j += 2;
                } else {
                    j += 1;
                }
            }
            if depth > 0 {
                result.push_str(&template[i..]);
                break;
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

// ─── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_inline_binding() {
        let w = synthesize_inline_workload("hello {{cycle}}").unwrap();
        assert_eq!(w.ops.len(), 1);
        assert_eq!(w.ops[0].name, "inline_0");
        let stmt = w.ops[0].op.get("stmt").unwrap().as_str().unwrap();
        assert_eq!(stmt, "hello {__inline_0}");
        match &w.ops[0].bindings {
            BindingsDef::PolydatSource(src) => {
                assert!(src.contains("input cycle: u64"));
                assert!(src.contains("__inline_0 := cycle"));
            }
            _ => panic!("expected PolydatSource bindings"),
        }
    }

    #[test]
    fn multiple_inline_bindings() {
        let w = synthesize_inline_workload(
            "id={{mod(hash(cycle), 100000)}} name={{number_to_words(cycle)}}",
        )
        .unwrap();
        assert_eq!(w.ops.len(), 1);
        let stmt = w.ops[0].op.get("stmt").unwrap().as_str().unwrap();
        assert_eq!(stmt, "id={__inline_0} name={__inline_1}");
        match &w.ops[0].bindings {
            BindingsDef::PolydatSource(src) => {
                assert!(src.contains("__inline_0 := mod(hash(cycle), 100000)"));
                assert!(src.contains("__inline_1 := number_to_words(cycle)"));
            }
            _ => panic!("expected PolydatSource bindings"),
        }
    }

    #[test]
    fn bindings_block_op_becomes_polydat_fields() {
        // A valid Polydat bindings block → one op whose fields are the
        // bound wire names, each resolved via `{name}`.
        let w =
            synthesize_inline_workload("x := cos(to_f64(cycle)); y := sin(to_f64(cycle))").unwrap();
        assert_eq!(w.ops.len(), 1);
        let keys: std::collections::BTreeSet<&str> =
            w.ops[0].op.keys().map(|s| s.as_str()).collect();
        assert!(keys.contains("x") && keys.contains("y"), "fields: {keys:?}");
        assert!(!w.ops[0].op.contains_key("stmt"), "should not be a text op");
        assert_eq!(w.ops[0].op.get("x").unwrap().as_str().unwrap(), "{x}");
        assert!(matches!(w.ops[0].bindings, BindingsDef::PolydatSource(_)));
    }

    #[test]
    fn bare_polydat_expr_becomes_out_field() {
        let w = synthesize_inline_workload("cos(to_f64(cycle))").unwrap();
        assert_eq!(w.ops.len(), 1);
        assert!(
            w.ops[0].op.contains_key("out"),
            "fields: {:?}",
            w.ops[0].op.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn invalid_polydat_falls_back_to_text_template() {
        // Has `:=` but doesn't compile → NOT adopted as Polydat; the
        // text-template form handles it instead (no panic, one op).
        let w = synthesize_inline_workload("x := not_a_real_fn(@@@)").unwrap();
        assert_eq!(w.ops.len(), 1);
        assert!(w.ops[0].op.contains_key("stmt"));
    }

    #[test]
    fn detection_is_compile_driven_not_syntactic() {
        // `{ref}` composite text isn't valid standalone Polydat → text
        // template (which still interpolates the ref).
        let w = synthesize_inline_workload("id-{cycle}").unwrap();
        assert!(w.ops[0].op.contains_key("stmt"));
    }

    #[test]
    fn helpers_split_and_name_bindings() {
        assert!(has_top_level_assignment("x := 1"));
        assert!(!has_top_level_assignment("hello {{x := 1}}")); // inside braces
        assert_eq!(split_top_level_semicolons("a := 1; b := 2").len(), 2);
        let src = build_polydat_candidate("a := 1; sin(cycle)");
        assert!(src.contains("a := 1"));
        assert!(src.contains("out := sin(cycle)")); // bare last → out
        assert_eq!(
            binding_wire_names("input cycle: u64\nt := 1\n__expr_0 := 2\nx := 3\n"),
            vec!["t".to_string(), "x".to_string()]
        ); // skips input + __
    }

    #[test]
    fn no_inline_bindings_plain_text() {
        let w = synthesize_inline_workload("hello world").unwrap();
        assert_eq!(w.ops.len(), 1);
        let stmt = w.ops[0].op.get("stmt").unwrap().as_str().unwrap();
        assert_eq!(stmt, "hello world");
        // Inline mode always emits the `input cycle: u64` line so
        // workloads referencing `{cycle}` validate cleanly. With no
        // inline expressions the bindings carry just that declaration
        // and nothing else.
        let bindings = match &w.ops[0].bindings {
            crate::model::BindingsDef::PolydatSource(s) => s.clone(),
            _ => panic!("expected PolydatSource"),
        };
        assert_eq!(bindings, "input cycle: u64\n");
    }

    #[test]
    fn reference_bind_points_preserved() {
        let w = synthesize_inline_workload("value={cycle}").unwrap();
        assert_eq!(w.ops.len(), 1);
        let stmt = w.ops[0].op.get("stmt").unwrap().as_str().unwrap();
        assert_eq!(stmt, "value={cycle}");
        // Same as `no_inline_bindings_plain_text`: a bare `{cycle}`
        // reference doesn't introduce inline expressions, but the
        // `input cycle: u64` convention line still gets emitted so
        // the workload-level placeholder validator recognises the
        // wire name.
        let bindings = match &w.ops[0].bindings {
            crate::model::BindingsDef::PolydatSource(s) => s.clone(),
            _ => panic!("expected PolydatSource"),
        };
        assert_eq!(bindings, "input cycle: u64\n");
    }

    #[test]
    fn semicolon_split_multiple_ops() {
        let w = synthesize_inline_workload("read {{cycle}};write {{mod(cycle, 100)}}").unwrap();
        assert_eq!(w.ops.len(), 2);
        assert_eq!(w.ops[0].name, "inline_0");
        assert_eq!(w.ops[1].name, "inline_1");
    }

    #[test]
    fn ratio_prefix() {
        let w = synthesize_inline_workload("3:read {{cycle}};1:write {{cycle}}").unwrap();
        assert_eq!(w.ops.len(), 2);
        assert_eq!(w.ops[0].params.get("ratio").unwrap().as_u64().unwrap(), 3);
        // ratio=1 is the default, so it's not stored explicitly.
        assert!(!w.ops[1].params.contains_key("ratio"));
    }

    #[test]
    fn ratio_one_not_stored() {
        let w = synthesize_inline_workload("hello {{cycle}}").unwrap();
        assert!(!w.ops[0].params.contains_key("ratio"));
    }

    #[test]
    fn duplicate_expressions_share_output() {
        let w = synthesize_inline_workload("a={{hash(cycle)}};b={{hash(cycle)}}").unwrap();
        // Both ops should reference the same __inline_0.
        let stmt0 = w.ops[0].op.get("stmt").unwrap().as_str().unwrap();
        let stmt1 = w.ops[1].op.get("stmt").unwrap().as_str().unwrap();
        assert_eq!(stmt0, "a={__inline_0}");
        assert_eq!(stmt1, "b={__inline_0}");
        match &w.ops[0].bindings {
            BindingsDef::PolydatSource(src) => {
                // Only one output for hash(cycle).
                let count = src.matches("__inline_").count();
                assert_eq!(count, 1);
            }
            _ => panic!("expected PolydatSource"),
        }
    }

    #[test]
    fn empty_op_is_error() {
        assert!(synthesize_inline_workload("").is_err());
        assert!(synthesize_inline_workload("   ").is_err());
    }

    #[test]
    fn mixed_reference_and_inline() {
        let w = synthesize_inline_workload("id={{mod(hash(cycle), 1000)}} raw={cycle}").unwrap();
        let stmt = w.ops[0].op.get("stmt").unwrap().as_str().unwrap();
        assert_eq!(stmt, "id={__inline_0} raw={cycle}");
    }
}
