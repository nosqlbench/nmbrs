// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! AST → canonical query string. Mirrors upstream's
//! `Expr.AppendString` family. Round-trip target:
//!
//!   pretty_string(parse(input)?) == upstream_canonical(input)
//!
//! Coverage parallels the parser — this first cut handles
//! [`MetricExpr`] (selectors). New cases land alongside parser
//! extensions; the parity tests light up incrementally.

use crate::ast::{
    AggrModifier, AggrModifierOp, BinaryOp, BinaryOpExpr, DurationExpr, Expr, FuncExpr,
    GroupModifier, GroupOp, JoinModifier, JoinOp, LabelFilter, LabelFilterOp, MetricExpr,
    NumberExpr, ParensExpr, RollupExpr, StringExpr, WithArgExpr, WithExpr,
};

/// Maximum length for a single line emitted by
/// [`pretty_print`]. Mirrors upstream's
/// `maxPrettifiedLineLen`.
const MAX_PRETTIFIED_LINE_LEN: usize = 80;

/// Multi-line pretty-print, mirroring upstream's `Prettify`.
/// Tries the single-line rendering first; if a node exceeds
/// [`MAX_PRETTIFIED_LINE_LEN`] it splits into multiple lines
/// per the upstream rules. Use this for human-readable
/// display; use [`pretty_string`] for canonical round-trip.
pub fn pretty_print(expr: &Expr) -> String {
    let mut out = String::new();
    append_prettified(&mut out, expr, 0, false);
    out
}

fn append_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

fn append_prettified(out: &mut String, e: &Expr, indent: usize, need_parens: bool) {
    let start = out.len();
    append_indent(out, indent);
    if need_parens {
        out.push('(');
    }
    append_expr(out, e);
    if need_parens {
        out.push(')');
    }
    if out.len() - start <= MAX_PRETTIFIED_LINE_LEN {
        return;
    }

    // Too long — split.
    out.truncate(start);
    let inner_indent = if need_parens {
        append_indent(out, indent);
        out.push_str("(\n");
        indent + 1
    } else {
        indent
    };
    match e {
        Expr::With(w) => {
            append_indent(out, inner_indent);
            out.push_str("WITH (\n");
            for b in &w.bindings {
                append_prettified_binding(out, b, inner_indent + 1);
                out.push_str(",\n");
            }
            append_indent(out, inner_indent);
            out.push_str(")\n");
            append_prettified(out, &w.body, inner_indent, false);
        }
        Expr::Binary(b) => {
            if b.keep_metric_names {
                append_indent(out, inner_indent);
                out.push_str("(\n");
            }
            let bi = if b.keep_metric_names {
                inner_indent + 1
            } else {
                inner_indent
            };
            let left_parens = needs_binary_arg_parens(&b.left);
            append_prettified(out, &b.left, bi, left_parens);
            out.push('\n');
            append_indent(out, bi + 1);
            append_binary_modifiers(out, b);
            out.push('\n');
            let right_parens = needs_right_parens(b);
            append_prettified(out, &b.right, bi, right_parens);
            if b.keep_metric_names {
                out.push('\n');
                append_indent(out, inner_indent);
                out.push_str(") keep_metric_names");
            }
        }
        Expr::Rollup(r) => {
            let inner_parens = matches!(*r.expr, Expr::Binary(_) | Expr::Rollup(_))
                || matches!(&*r.expr, Expr::Func(f) if f.modifier.is_some());
            append_prettified(out, &r.expr, inner_indent, inner_parens);
            append_rollup_modifiers(out, r);
        }
        Expr::Func(f) => {
            append_indent(out, inner_indent);
            append_escaped_ident(out, &f.name);
            append_prettified_func_args(out, inner_indent, &f.args);
            if let Some(m) = &f.modifier {
                // `append_aggr_modifier` already prepends a
                // space, matching the single-line form
                // `name(args) by(l)`. No extra space here.
                append_aggr_modifier(out, m);
            }
            if let Some(limit) = f.limit {
                out.push_str(&format!(" limit {}", limit));
            }
            if f.keep_metric_names {
                out.push_str(" keep_metric_names");
            }
        }
        Expr::Metric(me) => {
            append_prettified_metric(out, me, inner_indent);
        }
        _ => {
            // Other expression types fit on one line by
            // construction (literals, parens, etc.) — fall
            // back to the single-line rendering.
            append_indent(out, inner_indent);
            append_expr(out, e);
        }
    }
    if need_parens {
        out.push('\n');
        append_indent(out, indent);
        out.push(')');
    }
}

fn append_prettified_binding(out: &mut String, b: &WithArgExpr, indent: usize) {
    let start = out.len();
    append_indent(out, indent);
    append_with_binding(out, b);
    if out.len() - start <= MAX_PRETTIFIED_LINE_LEN {
        return;
    }
    // Too long — wrap the body in `( ... )` on its own lines.
    out.truncate(start);
    append_indent(out, indent);
    append_escaped_ident(out, &b.name);
    if !b.args.is_empty() {
        out.push('(');
        for (i, a) in b.args.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            append_escaped_ident(out, a);
        }
        out.push(')');
    }
    out.push_str(" = (\n");
    append_prettified(out, &b.expr, indent + 1, false);
    out.push('\n');
    append_indent(out, indent);
    out.push(')');
}

fn append_prettified_func_args(out: &mut String, indent: usize, args: &[Expr]) {
    out.push_str("(\n");
    for (i, arg) in args.iter().enumerate() {
        append_prettified(out, arg, indent + 1, false);
        if i + 1 < args.len() {
            out.push(',');
        }
        out.push('\n');
    }
    append_indent(out, indent);
    out.push(')');
}

fn append_prettified_metric(out: &mut String, me: &MetricExpr, indent: usize) {
    if me.label_filterss.is_empty() {
        append_indent(out, indent);
        out.push_str("{}");
        return;
    }
    let metric_name = get_metric_name(me);
    let only_name = is_only_metric_name(me);
    let name_was_quoted = metric_name_was_quoted(me)
        && metric_name
            .as_deref()
            .map(needs_quoted_form)
            .unwrap_or(false);
    let offset = if metric_name.is_some() { 1 } else { 0 };

    append_indent(out, indent);
    if let Some(name) = &metric_name
        && !name_was_quoted
    {
        append_escaped_ident(out, name);
    }
    if only_name {
        if name_was_quoted && let Some(name) = &metric_name {
            out.push('{');
            append_quoted_string(out, name);
            out.push('}');
        }
        return;
    }
    out.push_str("{\n");
    if name_was_quoted && let Some(name) = &metric_name {
        append_indent(out, 0);
        append_quoted_string(out, name);
        out.push_str(", \n");
    }
    let groups = &me.label_filterss;
    for (i, lfs) in groups.iter().enumerate() {
        if lfs.len() <= offset {
            continue;
        }
        append_prettified_label_filters(out, indent + 1, &lfs[offset..]);
        out.push('\n');
        if i + 1 < groups.len() && groups[i + 1].len() > offset {
            append_indent(out, indent + 2);
            out.push_str("or\n");
        }
    }
    append_indent(out, indent);
    out.push('}');
}

fn append_prettified_label_filters(out: &mut String, indent: usize, lfs: &[LabelFilter]) {
    let start = out.len();
    append_indent(out, indent);
    for (i, lf) in lfs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        append_label_filter(out, lf);
    }
    if out.len() - start <= MAX_PRETTIFIED_LINE_LEN {
        return;
    }
    out.truncate(start);
    for (i, lf) in lfs.iter().enumerate() {
        append_indent(out, indent);
        append_label_filter(out, lf);
        if i + 1 < lfs.len() {
            out.push_str(",\n");
        }
    }
}

fn append_binary_modifiers(out: &mut String, b: &BinaryOpExpr) {
    out.push_str(binary_op_str(b.op));
    if b.bool_modifier {
        out.push_str("bool");
    }
    if let Some(g) = &b.group_modifier {
        out.push(' ');
        append_group_modifier(out, g);
    }
    if let Some(j) = &b.join_modifier {
        out.push(' ');
        append_join_modifier(out, j);
        if let Some(prefix) = &b.join_modifier_prefix {
            out.push_str(" prefix ");
            append_expr(out, prefix);
        }
    }
}

/// Render an AST node to its canonical MetricsQL string.
pub fn pretty_string(expr: &Expr) -> String {
    let mut out = String::new();
    append_expr(&mut out, expr);
    out
}

fn append_expr(out: &mut String, expr: &Expr) {
    match expr {
        Expr::Metric(me) => append_metric(out, me),
        Expr::Rollup(re) => append_rollup(out, re),
        Expr::Number(n) => append_number(out, n),
        Expr::String(s) => append_string(out, s),
        Expr::Duration(d) => out.push_str(&d.value),
        Expr::Func(f) => append_func(out, f),
        Expr::Paren(p) => append_parens(out, p),
        Expr::Binary(b) => append_binary(out, b),
        Expr::With(w) => append_with(out, w),
    }
}

fn append_binary(out: &mut String, b: &BinaryOpExpr) {
    if b.keep_metric_names {
        out.push('(');
        append_binary_inner(out, b);
        out.push_str(") keep_metric_names");
    } else {
        append_binary_inner(out, b);
    }
}

fn append_binary_inner(out: &mut String, b: &BinaryOpExpr) {
    // Left operand — parens needed only when the operand is
    // itself a binary expr (precedence) or a rollup that
    // could be ambiguous (offset/@ suffixes).
    if needs_binary_arg_parens(&b.left) {
        out.push('(');
        append_expr(out, &b.left);
        out.push(')');
    } else {
        append_expr(out, &b.left);
    }
    out.push(' ');
    out.push_str(binary_op_str(b.op));
    // `bool` is fused to the op without a space — `==bool`,
    // `>=bool`, etc. (upstream appendModifiers).
    if b.bool_modifier {
        out.push_str("bool");
    }
    if let Some(g) = &b.group_modifier {
        out.push(' ');
        append_group_modifier(out, g);
    }
    if let Some(j) = &b.join_modifier {
        out.push(' ');
        append_join_modifier(out, j);
        if let Some(prefix) = &b.join_modifier_prefix {
            out.push_str(" prefix ");
            append_expr(out, prefix);
        }
    }
    out.push(' ');
    if needs_right_parens(b) {
        out.push('(');
        append_expr(out, &b.right);
        out.push(')');
    } else {
        append_expr(out, &b.right);
    }
}

/// Mirrors upstream `needBinaryOpArgParens`. Wraps:
///   - any binary subexpr (precedence + readability)
///   - rollups with `offset`/`@` modifiers (so suffix group
///     unambiguously)
///
/// Plain rollups (`metric[5m]`) don't need parens — the
/// `[…]` already groups the suffix to the operand.
fn needs_binary_arg_parens(e: &Expr) -> bool {
    match e {
        Expr::Binary(_) => true,
        Expr::Rollup(r) => r.offset.is_some() || r.at.is_some(),
        _ => false,
    }
}

/// Right-side-only wrapping. Mirrors upstream's
/// `needRightParens` — in addition to the generic args-parens
/// rule, the rhs of a binary op needs parens when it's a
/// metric or function whose name would be eaten by the
/// vector-matching grammar (`group_left`, `on`, `bool`,
/// `prefix`) or when a function carries `keep_metric_names`.
fn needs_right_parens(b: &BinaryOpExpr) -> bool {
    if needs_binary_arg_parens(&b.right) {
        return true;
    }
    match &*b.right {
        Expr::Metric(me) => metric_name_str(me)
            .map(is_reserved_binary_op_ident)
            .unwrap_or(false),
        Expr::Func(f) => {
            if is_reserved_binary_op_ident(&f.name) {
                return true;
            }
            // Only wrap when the rhs *itself* carries
            // `keep_metric_names`; the outer binary's flag
            // already implies a containing `(...)` wrap so
            // doubling up on the rhs is redundant — matches
            // the prettifier_test.go expected output for
            // `(... / ... sum(rate(x) keep_metric_names))
            //  keep_metric_names`.
            f.keep_metric_names
        }
        _ => false,
    }
}

/// Return the metric name regardless of how many other label
/// filters or `or` groups the expression has. Mirrors
/// upstream's `MetricExpr.getMetricName`. The name lives in
/// the first position of the first filter group (canonicalized
/// during parse).
fn metric_name_str(me: &MetricExpr) -> Option<&str> {
    let g = me.label_filterss.first()?;
    let lf = g.first()?;
    if lf.label != "__name__" || !matches!(lf.op, LabelFilterOp::Eq) {
        return None;
    }
    Some(lf.value.as_str())
}

fn is_reserved_binary_op_ident(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "on" | "ignoring" | "group_left" | "group_right" | "bool" | "prefix"
    )
}

fn append_group_modifier(out: &mut String, g: &GroupModifier) {
    out.push_str(match g.op {
        GroupOp::On => "on",
        GroupOp::Ignoring => "ignoring",
    });
    out.push('(');
    for (i, lbl) in g.labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        append_modifier_label(out, lbl);
    }
    out.push(')');
}

fn append_join_modifier(out: &mut String, j: &JoinModifier) {
    out.push_str(match j.op {
        JoinOp::GroupLeft => "group_left",
        JoinOp::GroupRight => "group_right",
    });
    out.push('(');
    for (i, lbl) in j.labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        append_modifier_label(out, lbl);
    }
    out.push(')');
}

fn binary_op_str(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Mod => "%",
        Pow => "^",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        And => "and",
        Or => "or",
        Unless => "unless",
        If => "if",
        IfNot => "ifnot",
        Default => "default",
        Atan2 => "atan2",
    }
}

fn append_func(out: &mut String, f: &FuncExpr) {
    append_escaped_ident(out, &f.name);
    out.push('(');
    // Function-call args use `, ` (upstream
    // appendStringArgListExpr).
    for (i, arg) in f.args.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        append_expr(out, arg);
    }
    out.push(')');
    if let Some(m) = &f.modifier {
        append_aggr_modifier(out, m);
    }
    if let Some(n) = f.limit {
        out.push_str(" limit ");
        let _ = std::fmt::Write::write_fmt(out, format_args!("{n}"));
    }
    if f.keep_metric_names {
        out.push_str(" keep_metric_names");
    }
}

fn append_aggr_modifier(out: &mut String, m: &AggrModifier) {
    out.push(' ');
    out.push_str(match m.op {
        AggrModifierOp::By => "by",
        AggrModifierOp::Without => "without",
    });
    out.push('(');
    // Modifier label lists use `,` (no space) — upstream
    // ModifierExpr.AppendString.
    for (i, lbl) in m.args.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        append_modifier_label(out, lbl);
    }
    out.push(')');
}

fn append_parens(out: &mut String, p: &ParensExpr) {
    out.push('(');
    // ParensExpr is rendered via the same `, `-separated
    // arg-list helper as function calls upstream.
    for (i, e) in p.exprs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        append_expr(out, e);
    }
    out.push(')');
}

/// Modifier labels preserve `*` literally (it's the "all
/// labels" wildcard inside `group_left(*)` etc.). All other
/// labels go through the standard escaped-ident path.
fn append_modifier_label(out: &mut String, lbl: &str) {
    if lbl == "*" {
        out.push('*');
    } else {
        append_escaped_ident(out, lbl);
    }
}

fn append_number(out: &mut String, n: &NumberExpr) {
    if !n.literal.is_empty() {
        out.push_str(&n.literal);
        return;
    }
    out.push_str(&format_go_g(n.value));
}

/// Mirrors Go's `strconv.FormatFloat(x, 'g', -1, 64)` — the
/// default `%g` formatter with shortest-round-trip precision.
/// We re-format synthesised numbers (folded constants) this
/// way so output matches upstream MetricsQL byte-for-byte.
fn format_go_g(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 {
            "+Inf".into()
        } else {
            "-Inf".into()
        };
    }
    if x == 0.0 {
        return if x.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    // Rust's `{:e}` produces a shortest-round-trip mantissa.
    // We use it to determine the exponent and significant
    // digit count, then choose between scientific and decimal
    // form per Go's rule: scientific when exp < -4 or
    // exp >= significant-digit-count.
    let sci = format!("{:e}", x);
    let (mantissa, exp_str) = sci.split_once('e').expect("e in scientific form");
    let exp: i32 = exp_str.parse().expect("exponent parses");
    // Go's `%g` with shortest precision (-1) compares the
    // exponent against a fixed `eprec = 6` when deciding
    // between scientific and decimal — NOT against the actual
    // digit count of the shortest representation.
    let eprec = 6;
    if exp < -4 || exp >= eprec {
        let sign = if exp >= 0 { '+' } else { '-' };
        format!("{}e{}{:02}", mantissa, sign, exp.abs())
    } else {
        // Rust's `{}` for finite f64 produces the shortest
        // unique decimal representation without scientific
        // shorthand — exactly what Go's `%g` falls back to for
        // exponents in this range.
        format!("{}", x)
    }
}

fn append_with(out: &mut String, w: &WithExpr) {
    out.push_str("WITH (");
    for (i, b) in w.bindings.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        append_with_binding(out, b);
    }
    out.push_str(") ");
    append_expr(out, &w.body);
}

fn append_with_binding(out: &mut String, b: &WithArgExpr) {
    append_escaped_ident(out, &b.name);
    if !b.args.is_empty() {
        out.push('(');
        for (i, a) in b.args.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            append_escaped_ident(out, a);
        }
        out.push(')');
    }
    out.push_str(" = ");
    append_expr(out, &b.expr);
}

fn append_string(out: &mut String, s: &StringExpr) {
    let quote = if s.single_quoted { '\'' } else { '"' };
    out.push(quote);
    for ch in s.value.chars() {
        // Inside the chosen quote style, escape only that
        // quote (the other quote round-trips literally).
        if ch == quote || ch == '\\' {
            out.push('\\');
        }
        if ch == '\n' {
            out.push('\\');
            out.push('n');
            continue;
        }
        if ch == '\t' {
            out.push('\\');
            out.push('t');
            continue;
        }
        if ch == '\r' {
            out.push('\\');
            out.push('r');
            continue;
        }
        out.push(ch);
    }
    out.push(quote);
}

fn append_rollup(out: &mut String, re: &RollupExpr) {
    // An aggregate-style FuncExpr with a `by`/`without`
    // modifier must be parenthesised before the rollup suffix
    // — `sum(x) by(l)[5m]` would otherwise parse as
    // `sum(x) by(l[5m])`.
    let needs_parens = matches!(*re.expr, Expr::Rollup(_) | Expr::Binary(_))
        || matches!(&*re.expr, Expr::Func(f) if f.modifier.is_some());
    if needs_parens {
        out.push('(');
    }
    append_expr(out, &re.expr);
    if needs_parens {
        out.push(')');
    }
    append_rollup_modifiers(out, re);
}

fn append_rollup_modifiers(out: &mut String, re: &RollupExpr) {
    if re.window.is_some() || re.step.is_some() || re.inherit_step {
        out.push('[');
        if let Some(w) = &re.window {
            append_duration(out, w);
        }
        if let Some(s) = &re.step {
            out.push(':');
            append_duration(out, s);
        } else if re.inherit_step {
            out.push(':');
        }
        out.push(']');
    }
    if let Some(off) = &re.offset {
        out.push_str(" offset ");
        append_duration(out, off);
    }
    if let Some(at) = &re.at {
        out.push_str(" @ ");
        let needs = matches!(**at, Expr::Binary(_));
        if needs {
            out.push('(');
        }
        append_expr(out, at);
        if needs {
            out.push(')');
        }
    }
}

fn append_duration(out: &mut String, d: &DurationExpr) {
    out.push_str(&d.value);
}

fn append_metric(out: &mut String, me: &MetricExpr) {
    if me.label_filterss.is_empty() {
        out.push_str("{}");
        return;
    }
    let metric_name = get_metric_name(me);
    let only_name = is_only_metric_name(me);
    // The leading `__name__` filter remembers whether the
    // source wrote it as a quoted string (`{"3foo"}`) or as a
    // bare identifier. Quoted form is preserved only when the
    // metric name actually needs quoting (contains chars that
    // aren't legal in a bare ident); otherwise we emit the
    // unquoted form regardless. The full `parse` path flips
    // the flag off during canonicalisation so post-expansion
    // output uses `\`-escapes per upstream's canonical
    // `MetricExpr.AppendString` path.
    let name_was_quoted = metric_name_was_quoted(me)
        && metric_name
            .as_deref()
            .map(needs_quoted_form)
            .unwrap_or(false);
    if let Some(name) = &metric_name
        && !name_was_quoted
    {
        append_escaped_ident(out, name);
        if only_name {
            return;
        }
    }
    let offset = if metric_name.is_some() { 1 } else { 0 };
    out.push('{');
    if name_was_quoted && let Some(name) = &metric_name {
        // Leading `"name"` slot — followed by a comma+space if
        // the first group has additional filters.
        append_quoted_string(out, name);
        let first_group = &me.label_filterss[0];
        if first_group.len() > 1 {
            out.push_str(", ");
        }
    }
    let mut first_group_emitted = false;
    let groups = &me.label_filterss;
    for lfs in groups.iter() {
        if lfs.len() <= offset {
            continue;
        }
        if first_group_emitted {
            out.push_str(" or ");
        }
        first_group_emitted = true;
        let slice = &lfs[offset..];
        for (j, lf) in slice.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            append_label_filter(out, lf);
        }
    }
    out.push('}');
}

fn append_label_filter(out: &mut String, lf: &LabelFilter) {
    // Template-ref filter (`{x, y="z"}`) — bare ident, no op
    // or value. Survives only on AST trees built by
    // `parse_for_prettify`; the full `parse` path expands it
    // away during WITH resolution.
    if lf.is_template_ref {
        append_escaped_ident(out, &lf.label);
        return;
    }
    if lf.was_quoted && needs_quoted_form(&lf.label) {
        append_quoted_string(out, &lf.label);
    } else {
        append_escaped_ident(out, &lf.label);
    }
    out.push_str(match lf.op {
        LabelFilterOp::Eq => "=",
        LabelFilterOp::Ne => "!=",
        LabelFilterOp::EqRegex => "=~",
        LabelFilterOp::NeRegex => "!~",
    });
    // Pre-expansion AST may carry a deferred value expression
    // (`m{foo=x+"y"}` from `parse_for_prettify`). Render it
    // as a flat `+`-separated chain matching the source
    // syntax — no spaces around the `+`, no parens around the
    // sub-binaries — instead of the canonical `Expr` form.
    if let Some(ve) = &lf.value_expr {
        append_label_value_chain(out, ve);
    } else {
        append_quoted_string(out, &lf.value);
    }
}

/// Flatten and emit a label-value `+` chain in source form
/// (`"foo"+s+"bar"`). Other shapes fall back to the generic
/// expression printer; in practice the parser only stores
/// strings and bare-ident WITH-template refs here.
fn append_label_value_chain(out: &mut String, e: &Expr) {
    if let Expr::Binary(b) = e
        && matches!(b.op, BinaryOp::Add)
    {
        append_label_value_chain(out, &b.left);
        out.push('+');
        append_label_value_chain(out, &b.right);
    } else {
        append_expr(out, e);
    }
}

/// Emit an identifier in canonical form. Identifiers whose
/// characters all qualify as ident-chars are emitted bare;
/// anything else gets `\` escapes per upstream's
/// `appendEscapedIdent`.
fn append_escaped_ident(out: &mut String, s: &str) {
    for (i, ch) in s.chars().enumerate() {
        let first = i == 0;
        let ok = if first {
            is_first_ident_char(ch)
        } else {
            is_ident_char(ch)
        };
        if ok {
            out.push(ch);
        } else {
            append_escape_sequence(out, ch);
        }
    }
}

fn append_escape_sequence(out: &mut String, ch: char) {
    out.push('\\');
    if !ch.is_control() {
        out.push(ch);
        return;
    }
    if (ch as u32) < 256 {
        out.push('x');
        out.push(to_hex(((ch as u32) >> 4) as u8 & 0xF));
        out.push(to_hex(((ch as u32) & 0xF) as u8));
    } else {
        out.push('u');
        let v = ch as u32;
        out.push(to_hex(((v >> 12) & 0xF) as u8));
        out.push(to_hex(((v >> 8) & 0xF) as u8));
        out.push(to_hex(((v >> 4) & 0xF) as u8));
        out.push(to_hex((v & 0xF) as u8));
    }
}

fn to_hex(n: u8) -> char {
    match n {
        0..=9 => char::from(b'0' + n),
        _ => char::from(b'a' + (n - 10)),
    }
}

fn append_quoted_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
}

fn is_first_ident_char(r: char) -> bool {
    r.is_alphabetic() || r == '_' || r == ':'
}

fn is_ident_char(r: char) -> bool {
    if is_first_ident_char(r) {
        return true;
    }
    if r == '.' {
        return true;
    }
    (r as u32) < 256 && r.is_ascii_digit()
}

/// Returns the bare metric name when `me` represents a
/// `metric{...}` form. Mirrors upstream `MetricExpr.getMetricName`.
/// True when a name has any character that wouldn't survive
/// as a bare identifier (start with a digit/non-letter,
/// contain punctuation, etc.). Mirrors upstream's
/// `hasEscapedChars`. Used together with `was_quoted` to
/// decide whether the prettifier should emit `"name"` quoted
/// form or `\`-escape form.
fn needs_quoted_form(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return true;
    };
    if !is_first_ident_char(first) {
        return true;
    }
    for ch in chars {
        if !is_ident_char(ch) {
            return true;
        }
    }
    false
}

fn metric_name_was_quoted(me: &MetricExpr) -> bool {
    me.label_filterss
        .first()
        .and_then(|g| g.first())
        .filter(|lf| lf.label == "__name__" && matches!(lf.op, LabelFilterOp::Eq))
        .map(|lf| lf.was_quoted)
        .unwrap_or(false)
}

fn get_metric_name(me: &MetricExpr) -> Option<String> {
    if me.label_filterss.is_empty() {
        return None;
    }
    let first = &me.label_filterss[0];
    if first.is_empty() || !is_metric_name_filter(&first[0]) {
        return None;
    }
    let name = &first[0].value;
    for lfs in me.label_filterss.iter().skip(1) {
        if lfs.is_empty() || !is_metric_name_filter(&lfs[0]) || &lfs[0].value != name {
            return None;
        }
    }
    Some(name.clone())
}

fn is_metric_name_filter(lf: &LabelFilter) -> bool {
    lf.label == "__name__" && matches!(lf.op, LabelFilterOp::Eq)
}

fn is_only_metric_name(me: &MetricExpr) -> bool {
    if get_metric_name(me).is_none() {
        return false;
    }
    me.label_filterss.iter().all(|lfs| lfs.len() <= 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{LabelFilter, LabelFilterOp, MetricExpr};

    fn make_metric(name: &str) -> Expr {
        Expr::Metric(MetricExpr {
            label_filterss: vec![vec![LabelFilter {
                label: "__name__".into(),
                op: LabelFilterOp::Eq,
                value: name.into(),
                is_template_ref: false,
                value_expr: None,
                was_quoted: false,
            }]],
        })
    }

    #[test]
    fn empty_selector() {
        let e = Expr::Metric(MetricExpr::default());
        assert_eq!(pretty_string(&e), "{}");
    }

    #[test]
    fn bare_metric() {
        assert_eq!(pretty_string(&make_metric("metric")), "metric");
    }

    #[test]
    fn metric_with_filter() {
        let e = Expr::Metric(MetricExpr {
            label_filterss: vec![vec![
                LabelFilter {
                    label: "__name__".into(),
                    op: LabelFilterOp::Eq,
                    value: "metric".into(),
                    is_template_ref: false,
                    was_quoted: false,
                    value_expr: None,
                },
                LabelFilter {
                    label: "foo".into(),
                    op: LabelFilterOp::Eq,
                    value: "bar".into(),
                    is_template_ref: false,
                    was_quoted: false,
                    value_expr: None,
                },
            ]],
        });
        assert_eq!(pretty_string(&e), r#"metric{foo="bar"}"#);
    }
}
