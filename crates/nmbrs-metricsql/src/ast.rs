// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! MetricsQL AST. Mirrors the node structure from upstream
//! `metricsql.Expr` so the prettifier can produce the same
//! canonical strings the Go round-trip tests assert.
//!
//! Filled in stages. The parser-test fixtures only exercise
//! variants that already exist; everything else stays a TODO
//! that the parser rejects with a `not_yet_supported` error
//! until we get to it.

/// Top-level MetricsQL expression. Shape mirrors upstream's
/// `metricsql.Expr` interface — every variant produces a
/// canonical string via [`crate::prettifier::pretty_string`].
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Numeric literal (`123`, `1.2e5`).
    Number(NumberExpr),
    /// String literal (`"foo"`, `'bar'`).
    String(StringExpr),
    /// Duration literal (`5m`, `1h30m`).
    Duration(DurationExpr),
    /// Selector: `metric{label="value", ...}[range:step] offset d @ at`.
    Metric(MetricExpr),
    /// Function call (transform / aggregate / rollup).
    Func(FuncExpr),
    /// Binary operation: `a + b`, `vec or vec`, etc.
    Binary(BinaryOpExpr),
    /// Parenthesised group (preserves precedence in pretty-print).
    Paren(ParensExpr),
    /// `WITH (alias = expr, ...) body` template form.
    With(WithExpr),
    /// Wraps an expression with a rollup suffix: range
    /// (`[5m]`), step (`[:3s]`), `offset`, and/or `@`.
    Rollup(RollupExpr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RollupExpr {
    pub expr: Box<Expr>,
    /// `[<window>:<step>]`. `window: None` means a step-only
    /// `[:3s]` form.
    pub window: Option<DurationExpr>,
    pub step: Option<DurationExpr>,
    /// True when the source had `[:]` or `[5m:]` — the step
    /// is left for the engine to fill in (inheriting from the
    /// outer query step). Round-trip-faithful: re-printing
    /// preserves the trailing colon.
    pub inherit_step: bool,
    /// `offset <duration>` (may be negative — the `-` is
    /// recorded inside the value string).
    pub offset: Option<DurationExpr>,
    /// `@ <expr>` modifier (timestamp anchor).
    pub at: Option<Box<Expr>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NumberExpr {
    pub value: f64,
    /// Original literal text (for round-trip — e.g. `1e5`
    /// shouldn't re-print as `100000`).
    pub literal: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StringExpr {
    pub value: String,
    /// Original source quote style. The Rust default is the
    /// canonical double-quote form; `'foo'` survives only on
    /// trees built by `parse_for_prettify` so the prettifier
    /// can round-trip the input exactly. `parse` rewrites
    /// every literal back to double quotes.
    pub single_quoted: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DurationExpr {
    pub value: String,
    /// `false` when the duration is interpreted as
    /// seconds-without-units (e.g. `[5]` rather than `[5s]`).
    pub requires_step: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetricExpr {
    /// Comma-separated label sets joined by `or`.
    pub label_filterss: Vec<Vec<LabelFilter>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LabelFilter {
    pub label: String,
    pub op: LabelFilterOp,
    pub value: String,
    /// True when this entry is a bare identifier appearing
    /// inside a `{...}` selector — a WITH template reference
    /// (`with (x={a="b"}) m{x}` → `m{a="b"}`). `label` carries
    /// the binding name; `op`/`value` are unused. Replaced by
    /// the binding's filter set during WITH expansion;
    /// production AST should never carry one.
    pub is_template_ref: bool,
    /// True when the label/metric name was originally written
    /// in the Prometheus 3.x quoted form (`{"3foo"=...}`).
    /// The prettifier preserves the quoted form when this is
    /// set; the post-expansion canonical printer ignores it
    /// and always uses `\`-escapes (matching upstream's
    /// canonical `MetricExpr.AppendString` path).
    pub was_quoted: bool,
    /// Deferred value expression for label values that mix
    /// strings with WITH template refs (`m{foo=x+"y"}`). When
    /// present, takes precedence over `value` until WITH
    /// expansion + constant folding collapse it back to a
    /// plain string. Production AST has this set to `None`.
    pub value_expr: Option<Box<Expr>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelFilterOp {
    Eq,      // =
    Ne,      // !=
    EqRegex, // =~
    NeRegex, // !~
}

#[derive(Debug, Clone, PartialEq)]
pub struct FuncExpr {
    pub name: String,
    pub args: Vec<Expr>,
    pub keep_metric_names: bool,
    /// Aggregate-style `by (label, ...)` or
    /// `without (label, ...)` modifier. `None` for non-
    /// aggregate calls.
    pub modifier: Option<AggrModifier>,
    /// Aggregate-style `limit N` modifier — caps the number
    /// of result series. `None` when absent.
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AggrModifier {
    pub op: AggrModifierOp,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggrModifierOp {
    By,
    Without,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BinaryOpExpr {
    pub op: BinaryOp,
    pub left: Box<Expr>,
    pub right: Box<Expr>,
    pub bool_modifier: bool,
    pub group_modifier: Option<GroupModifier>,
    pub join_modifier: Option<JoinModifier>,
    /// `prefix <expr>` after a join modifier — augments the
    /// labels brought across a vector match. Held as a generic
    /// expression so WITH-template references (`prefix x`,
    /// `prefix "foo"+x`) can be expanded into a string after
    /// substitution. Production AST should always carry an
    /// `Expr::String`.
    pub join_modifier_prefix: Option<Box<Expr>>,
    /// `(expr) keep_metric_names` form. The original parens
    /// are synthesised on output by the prettifier when this
    /// flag is set.
    pub keep_metric_names: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Unless,
    If,
    IfNot,
    Default,
    Atan2,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupModifier {
    /// `on` or `ignoring`.
    pub op: GroupOp,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupOp {
    On,
    Ignoring,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JoinModifier {
    /// `group_left` or `group_right`.
    pub op: JoinOp,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinOp {
    GroupLeft,
    GroupRight,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParensExpr {
    pub exprs: Vec<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WithExpr {
    pub bindings: Vec<WithArgExpr>,
    pub body: Box<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WithArgExpr {
    pub name: String,
    pub args: Vec<String>,
    pub expr: Expr,
}
