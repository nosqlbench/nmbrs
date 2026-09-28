// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! MetricsQL parser. Token stream → [`crate::ast::Expr`].
//!
//! This is the first cut of the recursive-descent port. It
//! covers the simplest selector forms the round-trip parity
//! tests exercise:
//!
//!   - bare metric name (`metric`)
//!   - empty selector (`{}`)
//!   - selector with label filters (`{a="b"}`,
//!     `metric{a="b"}`, `{a="b",c!="d"}`)
//!   - the four label-filter ops `=`, `!=`, `=~`, `!~`
//!
//! Subsequent passes will add: range/subquery suffixes
//! (`[5m]`, `[5m:1s]`), `offset`, the `@` modifier, the
//! `or` filter combinator, function calls, binary ops,
//! aggregations, `WITH` templates. Each new feature is
//! verified by re-running the parity tests with
//! `RUN_METRICSQL_PARITY=1` and watching the pass count
//! grow.

use crate::ast::{
    AggrModifier, AggrModifierOp, BinaryOp, BinaryOpExpr, DurationExpr, Expr, FuncExpr,
    GroupModifier, GroupOp, JoinModifier, JoinOp, LabelFilter, LabelFilterOp, MetricExpr,
    NumberExpr, ParensExpr, RollupExpr, StringExpr, WithArgExpr, WithExpr,
};
use crate::lexer::{LexError, Token, lex};

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub at: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "parse error at byte {}: {}", self.at, self.message)
    }
}

impl std::error::Error for ParseError {}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        ParseError {
            message: e.message,
            at: e.at,
        }
    }
}

/// Parse a MetricsQL query into an [`Expr`] AST.
pub fn parse(input: &str) -> Result<Expr, ParseError> {
    let tokens = lex(input)?;
    let mut p = Parser { tokens, pos: 0 };
    let expr = p.parse_expr()?;
    p.expect_eof()?;
    let defaults = default_with_arg_exprs()?;
    let expr = expand_with_expr(&defaults, expr)?;
    let expr = remove_parens_expr(expr);
    let expr = simplify_constants(expr);
    Ok(strip_quoted_label_flags(expr))
}

/// Drop the `was_quoted` flag from every label filter under
/// the tree. The full `parse` path emits canonical strings
/// using `\`-escapes; only `parse_for_prettify` keeps the
/// quoted form for round-trip output.
fn strip_quoted_label_flags(e: Expr) -> Expr {
    match e {
        Expr::Metric(mut m) => {
            for g in m.label_filterss.iter_mut() {
                for lf in g.iter_mut() {
                    lf.was_quoted = false;
                }
            }
            Expr::Metric(m)
        }
        Expr::String(mut s) => {
            // Canonical form is double-quoted; the
            // round-trip-only `single_quoted` flag survives
            // only on `parse_for_prettify` trees.
            s.single_quoted = false;
            Expr::String(s)
        }
        Expr::Rollup(mut r) => {
            let inner = std::mem::replace(
                &mut *r.expr,
                Expr::Number(NumberExpr {
                    value: 0.0,
                    literal: "0".into(),
                }),
            );
            r.expr = Box::new(strip_quoted_label_flags(inner));
            if let Some(at) = r.at.take() {
                r.at = Some(Box::new(strip_quoted_label_flags(*at)));
            }
            Expr::Rollup(r)
        }
        Expr::Func(mut f) => {
            f.args = f.args.into_iter().map(strip_quoted_label_flags).collect();
            Expr::Func(f)
        }
        Expr::Binary(mut b) => {
            let l = std::mem::replace(
                &mut *b.left,
                Expr::Number(NumberExpr {
                    value: 0.0,
                    literal: "0".into(),
                }),
            );
            let r = std::mem::replace(
                &mut *b.right,
                Expr::Number(NumberExpr {
                    value: 0.0,
                    literal: "0".into(),
                }),
            );
            b.left = Box::new(strip_quoted_label_flags(l));
            b.right = Box::new(strip_quoted_label_flags(r));
            if let Some(p) = b.join_modifier_prefix.take() {
                b.join_modifier_prefix = Some(Box::new(strip_quoted_label_flags(*p)));
            }
            Expr::Binary(b)
        }
        Expr::Paren(mut p) => {
            p.exprs = p.exprs.into_iter().map(strip_quoted_label_flags).collect();
            Expr::Paren(p)
        }
        other => other,
    }
}

/// Parse without WITH expansion or constant folding — the
/// shape upstream's `Prettify` operates on. The returned tree
/// preserves `WithExpr` nodes and unfolded literal-binary
/// subtrees so the prettifier can re-emit the original
/// `WITH (...)` template form. Use [`parse`] for evaluation;
/// use this only when round-tripping the source for display.
pub fn parse_for_prettify(input: &str) -> Result<Expr, ParseError> {
    let tokens = lex(input)?;
    let mut p = Parser { tokens, pos: 0 };
    let expr = p.parse_expr()?;
    p.expect_eof()?;
    Ok(remove_parens_expr(expr))
}

/// Built-in WITH macros made available implicitly to every
/// query, mirroring upstream's `getDefaultWithArgExprs`. The
/// list is small enough to parse on demand; we cache nothing
/// since `parse` is the only caller and the cost is trivial.
fn default_with_arg_exprs() -> Result<Vec<WithArgExpr>, ParseError> {
    const SOURCES: &[&str] = &[
        // resource utilization
        "ru(freev, maxv) = clamp_min(maxv - clamp_min(freev, 0), 0) / clamp_min(maxv, 0) * 100",
        // time to fuckup
        "ttf(freev) = smooth_exponential(\
            clamp_max(clamp_max(-freev, 0) / clamp_max(deriv_fast(freev), 0), 365*24*3600),\
            clamp_max(step()/300, 1)\
        )",
        "range_median(q) = range_quantile(0.5, q)",
        "alias(q, name) = label_set(q, \"__name__\", name)",
    ];
    let mut out: Vec<WithArgExpr> = Vec::with_capacity(SOURCES.len());
    for src in SOURCES {
        // Each source is one `name(args) = expr` binding —
        // wrap it in a `with (...) 0` shell so we can reuse
        // the existing WITH-binding parser.
        let wrapped = format!("with ({src}) 0");
        let toks = lex(&wrapped)?;
        let mut wp = Parser {
            tokens: toks,
            pos: 0,
        };
        let e = wp.parse_expr()?;
        wp.expect_eof()?;
        let Expr::With(w) = e else {
            return Err(ParseError {
                message: format!("default WITH source did not parse as a WITH expr: {src}"),
                at: 0,
            });
        };
        out.extend(w.bindings.into_iter());
    }
    Ok(out)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn advance(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if !t.is_eof() {
            self.pos += 1;
        }
        t
    }

    #[allow(dead_code)]
    fn expect(&mut self, raw: &str) -> Result<Token, ParseError> {
        let t = self.peek();
        if t.raw != raw {
            return Err(ParseError {
                message: format!("expected {raw:?}, got {:?}", t.raw),
                at: t.start,
            });
        }
        Ok(self.advance())
    }

    fn expect_eof(&mut self) -> Result<(), ParseError> {
        let t = self.peek();
        if !t.is_eof() {
            return Err(ParseError {
                message: format!("unexpected trailing token {:?}", t.raw),
                at: t.start,
            });
        }
        Ok(())
    }

    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.parse_single_expr()?;
        loop {
            let op = match parse_binary_op(&self.peek().raw) {
                Some(op) => op,
                None => return Ok(e),
            };
            self.advance();
            let bool_modifier = self.peek().raw.eq_ignore_ascii_case("bool");
            if bool_modifier {
                self.advance();
            }
            // Optional group modifier: `on(labels)` /
            // `ignoring(labels)`.
            let group_modifier = self.maybe_parse_group_modifier()?;
            // Optional join modifier: `group_left(labels)` /
            // `group_right(labels)`. Only valid after a group
            // modifier.
            let join_modifier = if group_modifier.is_some() {
                self.maybe_parse_join_modifier()?
            } else {
                None
            };
            // Optional `prefix <string-expr>` after a join
            // modifier. The expression is restricted to
            // strings or bare-ident WITH-template refs joined
            // by `+`, so we don't accidentally swallow the
            // right operand of the outer binary op.
            let mut join_modifier_prefix: Option<Box<Expr>> = None;
            if join_modifier.is_some() && self.peek().raw.eq_ignore_ascii_case("prefix") {
                self.advance();
                join_modifier_prefix = Some(Box::new(self.parse_prefix_expr()?));
            }
            let right = self.parse_single_expr()?;
            // `(left op right) keep_metric_names` — the keyword
            // after the rhs propagates onto the BinaryOpExpr.
            // The prettifier round-trips this by wrapping the
            // node in parens followed by the keyword.
            let mut keep_metric_names = false;
            if self.peek().raw.eq_ignore_ascii_case("keep_metric_names") {
                keep_metric_names = true;
                self.advance();
            }
            let be = BinaryOpExpr {
                op,
                left: Box::new(e),
                right: Box::new(right),
                bool_modifier,
                group_modifier,
                join_modifier,
                join_modifier_prefix,
                keep_metric_names,
            };
            e = balance_binary_op(be);
        }
    }

    fn maybe_parse_group_modifier(&mut self) -> Result<Option<GroupModifier>, ParseError> {
        let op = match self.peek().raw.to_ascii_lowercase().as_str() {
            "on" => GroupOp::On,
            "ignoring" => GroupOp::Ignoring,
            _ => return Ok(None),
        };
        self.advance();
        let labels = self.parse_paren_label_list()?;
        Ok(Some(GroupModifier { op, labels }))
    }

    fn maybe_parse_join_modifier(&mut self) -> Result<Option<JoinModifier>, ParseError> {
        let op = match self.peek().raw.to_ascii_lowercase().as_str() {
            "group_left" => JoinOp::GroupLeft,
            "group_right" => JoinOp::GroupRight,
            _ => return Ok(None),
        };
        self.advance();
        let labels = if self.peek().raw == "(" {
            self.parse_paren_label_list()?
        } else {
            Vec::new()
        };
        Ok(Some(JoinModifier { op, labels }))
    }

    /// `( label [, label]* )` or `()` (empty list) or
    /// `(*)` (wildcard, stored as a single `*` entry).
    fn parse_paren_label_list(&mut self) -> Result<Vec<String>, ParseError> {
        self.expect("(")?;
        let mut labels: Vec<String> = Vec::new();
        if self.peek().raw == ")" {
            self.advance();
            return Ok(labels);
        }
        loop {
            let t = self.advance();
            if t.is_eof() {
                return Err(ParseError {
                    message: "unexpected EOF in label list".into(),
                    at: t.start,
                });
            }
            // Quoted label name (Prometheus 3.x UTF-8 form):
            // `"cluster!one"` → unquote to the raw label text;
            // the prettifier re-escapes special chars on output.
            let label = if is_string_token(&t) {
                unquote_string(&t.raw)?
            } else {
                unescape_ident(&t.raw)
            };
            labels.push(label);
            match self.peek().raw.as_str() {
                "," => {
                    self.advance();
                    // Trailing comma: `on (a, b,)` is allowed.
                    if self.peek().raw == ")" {
                        self.advance();
                        break;
                    }
                }
                ")" => {
                    self.advance();
                    break;
                }
                other => {
                    return Err(ParseError {
                        message: format!("expected ',' or ')' in label list, got {other:?}"),
                        at: self.peek().start,
                    });
                }
            }
        }
        Ok(labels)
    }

    /// Single operand in a binary expression — handles unary
    /// `+`/`-`, then dispatches to literal / metric / parens /
    /// function. The optional rollup suffix wraps the result
    /// when the input had `[...]`, `offset`, or `@`.
    fn parse_single_expr(&mut self) -> Result<Expr, ParseError> {
        // Unary `-` / `+`. Upstream rewrites `-expr` as
        // `0 - expr` then runs constant-folding to collapse
        // `0 - <literal>` back to a single negative number.
        // We do the same in one step for the literal case
        // (the round-trip tests expect `-1` to stay `-1`).
        if self.peek().raw == "-" {
            self.advance();
            let inner = self.parse_single_expr()?;
            // Upstream represents `-expr` as `0 - expr` so that
            // the precedence balancer rotates correctly when
            // the operand is a binary op (e.g. `-1 ^ 0.5` must
            // parse as `-(1^0.5)` not `(-1)^0.5`). Constant
            // folding then collapses `0 - <number>` back to a
            // single literal.
            let zero = Expr::Number(NumberExpr {
                value: 0.0,
                literal: "0".into(),
            });
            return Ok(Expr::Binary(BinaryOpExpr {
                op: BinaryOp::Sub,
                left: Box::new(zero),
                right: Box::new(inner),
                bool_modifier: false,
                group_modifier: None,
                join_modifier: None,
                join_modifier_prefix: None,
                keep_metric_names: false,
            }));
        }
        if self.peek().raw == "+" {
            self.advance();
            return self.parse_single_expr();
        }
        let inner = self.parse_single_expr_without_rollup_suffix()?;
        self.maybe_parse_rollup(inner)
    }

    /// Dispatch on the next token: number, string, duration,
    /// `(` group, `{` selector, or identifier-led metric expr.
    fn parse_single_expr_without_rollup_suffix(&mut self) -> Result<Expr, ParseError> {
        let tok = self.peek().clone();
        if tok.is_eof() {
            return Err(ParseError {
                message: "expected expression, got EOF".into(),
                at: tok.start,
            });
        }
        // Standalone string literal.
        if is_string_token(&tok) {
            self.advance();
            let single_quoted = tok.raw.starts_with('\'');
            let value = unquote_string(&tok.raw)?;
            return Ok(Expr::String(StringExpr {
                value,
                single_quoted,
            }));
        }
        // Grafana-style `$__interval` / `$__rate_interval`
        // placeholders. Upstream substitutes both with the
        // inherited-step duration `1i`. Checked BEFORE the
        // generic duration arm because the lexer treats the
        // placeholder as a duration token (so `$__interval`
        // would otherwise survive into the AST verbatim).
        if is_dollar_interval(&tok.raw) {
            self.advance();
            return Ok(Expr::Duration(DurationExpr {
                value: "1i".into(),
                requires_step: false,
            }));
        }
        // Standalone duration literal (`1h`, `0.34h4m5s`).
        // Has to be checked BEFORE the number arm since the
        // lexer emits the whole `1h` as a single token.
        if is_full_duration_literal(&tok.raw) {
            self.advance();
            return Ok(Expr::Duration(DurationExpr {
                value: tok.raw,
                requires_step: false,
            }));
        }
        // Standalone number literal — including `Inf` / `NaN`
        // identifiers, which upstream treats as numbers.
        if is_number_prefix(&tok.raw) || is_inf_or_nan(&tok.raw) {
            self.advance();
            let value = parse_number_literal(&tok.raw)?;
            return Ok(Expr::Number(NumberExpr {
                value,
                literal: tok.raw,
            }));
        }
        // `{...}` selector.
        if tok.raw == "{" {
            return self.parse_metric_expr();
        }
        // `(...)` parenthesised group — comma-separated
        // expressions inside, mirroring upstream's parensExpr.
        if tok.raw == "(" {
            return self.parse_parens_expr();
        }
        // Identifier — could be a bare metric name OR a
        // function call (`name(...)`). We have to look ahead
        // one token to disambiguate.
        if is_ident_token(&tok) {
            // `with (...) body` template form — case-insensitive
            // keyword; consumes the second `(` so it must be
            // checked before the generic `name(...)` arm.
            if tok.raw.eq_ignore_ascii_case("with") {
                let second = self.tokens.get(self.pos + 1).cloned().unwrap_or(Token {
                    raw: String::new(),
                    start: 0,
                });
                if second.raw == "(" {
                    return self.parse_with_expr();
                }
            }
            // Peek the second token without committing.
            let second = self.tokens.get(self.pos + 1).cloned().unwrap_or(Token {
                raw: String::new(),
                start: 0,
            });
            if second.raw == "(" {
                return self.parse_func_call();
            }
            // `sum by (l) (...)` / `sum without (l) (...)` —
            // aggregate function name followed by a modifier.
            // Only valid for known aggregate function names so
            // we don't misread `metric by` as an aggregate.
            let ident = unescape_ident(&tok.raw);
            if is_aggr_func(&ident)
                && (second.raw.eq_ignore_ascii_case("by")
                    || second.raw.eq_ignore_ascii_case("without"))
            {
                return self.parse_func_call();
            }
            return self.parse_metric_expr();
        }
        Err(ParseError {
            message: format!("expected expression, got {:?}", tok.raw),
            at: tok.start,
        })
    }

    /// If the current token is a rollup-start (`[`, `offset`,
    /// `@`), consume the modifiers and wrap `inner` in a
    /// [`RollupExpr`]. Otherwise return `inner` unchanged.
    fn maybe_parse_rollup(&mut self, inner: Expr) -> Result<Expr, ParseError> {
        if !is_rollup_start(self.peek()) {
            return Ok(inner);
        }
        let mut re = RollupExpr {
            expr: Box::new(inner),
            window: None,
            step: None,
            inherit_step: false,
            offset: None,
            at: None,
        };
        if self.peek().raw == "[" {
            let (w, s, inherit) = self.parse_window_and_step()?;
            re.window = w;
            re.step = s;
            re.inherit_step = inherit;
        }
        // Upstream: `@` may come either before or after `offset`.
        // The two-pass check at the same call site mirrors that.
        if self.peek().raw == "@" {
            re.at = Some(Box::new(self.parse_at_expr()?));
        }
        if is_offset_keyword(&self.peek().raw) {
            re.offset = Some(self.parse_offset()?);
        }
        if self.peek().raw == "@" {
            if re.at.is_some() {
                return Err(ParseError {
                    message: "duplicate `@` token".into(),
                    at: self.peek().start,
                });
            }
            re.at = Some(Box::new(self.parse_at_expr()?));
        }
        Ok(Expr::Rollup(re))
    }

    /// `[ <window>? ( ':' <step>? )? ]`
    fn parse_window_and_step(
        &mut self,
    ) -> Result<(Option<DurationExpr>, Option<DurationExpr>, bool), ParseError> {
        let _open = self.expect("[")?;

        let mut window: Option<DurationExpr> = None;
        let mut step: Option<DurationExpr> = None;
        let mut inherit_step = false;

        // Window component (absent when token starts with `:`
        // or is `]`).
        let tok = self.peek().clone();
        if !tok.raw.starts_with(':') && tok.raw != "]" {
            if is_dollar_interval(&tok.raw) {
                // Upstream skips `$__interval`/`$__rate_interval`
                // as "treat the window as missing"; the engine
                // fills it in at evaluation time.
                self.advance();
            } else if let Some(colon) = tok.raw.find(':')
                && colon > 0
            {
                // Lexer treats `:` as an ident continuation
                // char (so metric names like `node:cpu` work),
                // which means `[w1:w2]` lands here as a single
                // `w1:w2` token. Split on the first `:` so the
                // pieces become window + step independently —
                // matching upstream's behaviour.
                self.advance();
                let (w, s) = tok.raw.split_at(colon);
                window = Some(DurationExpr {
                    value: w.into(),
                    requires_step: false,
                });
                let s_after = &s[1..]; // skip the colon
                if !s_after.is_empty() {
                    step = Some(DurationExpr {
                        value: s_after.into(),
                        requires_step: false,
                    });
                } else if self.peek().raw == "]" {
                    inherit_step = true;
                }
                self.expect("]")?;
                return Ok((window, step, inherit_step));
            } else {
                window = Some(self.parse_positive_duration()?);
            }
        }

        // Step component (token starts with `:`).
        let tok = self.peek().clone();
        if tok.raw.starts_with(':') {
            let after_colon = tok.raw[1..].to_string();
            self.advance();
            if after_colon.is_empty() {
                // Bare `:`. Either `[5m:]` (inherit) or
                // `[5m: 3s]` with whitespace after the colon.
                if self.peek().raw == "]" {
                    inherit_step = true;
                } else {
                    step = Some(self.parse_positive_duration()?);
                }
            } else {
                step = Some(DurationExpr {
                    value: after_colon,
                    requires_step: false,
                });
            }
        }

        self.expect("]")?;
        Ok((window, step, inherit_step))
    }

    fn parse_positive_duration(&mut self) -> Result<DurationExpr, ParseError> {
        let tok = self.advance();
        if tok.raw.is_empty() {
            return Err(ParseError {
                message: "expected duration".into(),
                at: tok.start,
            });
        }
        // The token may be a duration literal (`5m`), a bare
        // number used as seconds (`30`), or — when reached via
        // a rollup-step token like `:1s` already split off —
        // the residue after the leading `:`.
        Ok(DurationExpr {
            value: tok.raw,
            requires_step: false,
        })
    }

    fn parse_offset(&mut self) -> Result<DurationExpr, ParseError> {
        if !is_offset_keyword(&self.peek().raw) {
            return Err(ParseError {
                message: format!("expected `offset`, got {:?}", self.peek().raw),
                at: self.peek().start,
            });
        }
        self.advance();
        let mut negative = false;
        if self.peek().raw == "-" {
            negative = true;
            self.advance();
        } else if self.peek().raw == "+" {
            self.advance();
        }
        // `$__interval` and `$__rate_interval` resolve to a
        // single inherited step (`1i`) in MetricsQL — they're
        // Grafana-style placeholders the parser substitutes
        // directly.
        let tok = self.peek().clone();
        if is_dollar_interval(&tok.raw) {
            self.advance();
            let value = if negative { "-1i".into() } else { "1i".into() };
            return Ok(DurationExpr {
                value,
                requires_step: false,
            });
        }
        let mut d = self.parse_positive_duration()?;
        if negative {
            d.value = format!("-{}", d.value);
        }
        Ok(d)
    }

    fn parse_parens_expr(&mut self) -> Result<Expr, ParseError> {
        self.expect("(")?;
        let mut exprs: Vec<Expr> = Vec::new();
        if self.peek().raw == ")" {
            self.advance();
            // `keep_metric_names` after `()` legal but we
            // discard it for now (parens with no exprs is
            // unusual).
            self.maybe_consume_keep_metric_names();
            return Ok(Expr::Paren(ParensExpr { exprs }));
        }
        loop {
            let e = self.parse_expr()?;
            exprs.push(e);
            match self.peek().raw.as_str() {
                "," => {
                    self.advance();
                    // Trailing comma: `(a, b,)` is allowed.
                    if self.peek().raw == ")" {
                        self.advance();
                        break;
                    }
                }
                ")" => {
                    self.advance();
                    break;
                }
                other => {
                    return Err(ParseError {
                        message: format!("expected ',' or ')' in parens, got {other:?}"),
                        at: self.peek().start,
                    });
                }
            }
        }
        // `(expr) keep_metric_names` — when the parens
        // contain exactly one binary expr, propagate the flag
        // onto it and unwrap the parens (matches upstream's
        // storage shape, which records KeepMetricNames on
        // BinaryOpExpr). For a non-binary inner expr, leave
        // the keyword for the outer parser — it likely belongs
        // to a containing binary op like
        // `... + (Func keep_metric_names) keep_metric_names`.
        if exprs.len() == 1
            && matches!(&exprs[0], Expr::Binary(_))
            && self.peek().raw.eq_ignore_ascii_case("keep_metric_names")
        {
            self.advance();
            let single = exprs.pop().unwrap();
            if let Expr::Binary(mut b) = single {
                b.keep_metric_names = true;
                return Ok(Expr::Binary(b));
            }
        }
        Ok(Expr::Paren(ParensExpr { exprs }))
    }

    /// `WITH (a = expr, b(x) = expr2, ...) body`. The bindings
    /// substitute named references inside `body` at expansion
    /// time. The result expression is built with all bindings
    /// resolved — `WithExpr` nodes shouldn't survive into the
    /// final tree.
    fn parse_with_expr(&mut self) -> Result<Expr, ParseError> {
        let with_tok = self.advance();
        if !with_tok.raw.eq_ignore_ascii_case("with") {
            return Err(ParseError {
                message: format!("expected `with`, got {:?}", with_tok.raw),
                at: with_tok.start,
            });
        }
        self.expect("(")?;
        let mut bindings: Vec<WithArgExpr> = Vec::new();
        if self.peek().raw == ")" {
            self.advance();
        } else {
            loop {
                let wa = self.parse_with_arg_expr()?;
                bindings.push(wa);
                match self.peek().raw.as_str() {
                    "," => {
                        self.advance();
                        if self.peek().raw == ")" {
                            self.advance();
                            break;
                        }
                    }
                    ")" => {
                        self.advance();
                        break;
                    }
                    other => {
                        return Err(ParseError {
                            message: format!("expected ',' or ')' in WITH bindings, got {other:?}"),
                            at: self.peek().start,
                        });
                    }
                }
            }
        }
        let body = self.parse_expr()?;
        Ok(Expr::With(WithExpr {
            bindings,
            body: Box::new(body),
        }))
    }

    fn parse_with_arg_expr(&mut self) -> Result<WithArgExpr, ParseError> {
        let name_tok = self.advance();
        if !is_ident_token(&name_tok) {
            return Err(ParseError {
                message: format!("expected ident in WITH binding, got {:?}", name_tok.raw),
                at: name_tok.start,
            });
        }
        let name = unescape_ident(&name_tok.raw);
        let mut args: Vec<String> = Vec::new();
        if self.peek().raw == "(" {
            // `name(arg1, arg2, ...) = expr` — function template.
            self.advance();
            if self.peek().raw != ")" {
                loop {
                    let t = self.advance();
                    if !is_ident_token(&t) {
                        return Err(ParseError {
                            message: format!(
                                "expected ident in WITH binding args, got {:?}",
                                t.raw
                            ),
                            at: t.start,
                        });
                    }
                    args.push(unescape_ident(&t.raw));
                    match self.peek().raw.as_str() {
                        "," => {
                            self.advance();
                            if self.peek().raw == ")" {
                                break;
                            }
                        }
                        ")" => break,
                        other => {
                            return Err(ParseError {
                                message: format!(
                                    "expected ',' or ')' in WITH binding args, got {other:?}"
                                ),
                                at: self.peek().start,
                            });
                        }
                    }
                }
            }
            self.expect(")")?;
        }
        if self.peek().raw != "=" {
            return Err(ParseError {
                message: format!(
                    "expected `=` in WITH binding for {:?}, got {:?}",
                    name,
                    self.peek().raw
                ),
                at: self.peek().start,
            });
        }
        self.advance();
        let expr = self.parse_expr()?;
        Ok(WithArgExpr { name, args, expr })
    }

    fn maybe_consume_keep_metric_names(&mut self) -> bool {
        if self.peek().raw.eq_ignore_ascii_case("keep_metric_names") {
            self.advance();
            true
        } else {
            false
        }
    }

    fn parse_func_call(&mut self) -> Result<Expr, ParseError> {
        let name_tok = self.advance();
        let mut name = unescape_ident(&name_tok.raw);
        let is_aggr = is_aggr_func(&name);
        // Aggregates are case-insensitive — canonicalize the
        // name to lowercase so `SUM(x)` round-trips as `sum(x)`.
        if is_aggr {
            name = name.to_ascii_lowercase();
        }
        // Aggregate `by`/`without` modifier may appear BEFORE
        // the arg list: `sum by (l) (x)`. Parse it pre-emptively
        // for known aggregate names so the caller doesn't see
        // an unexpected token after the bare identifier.
        let mut modifier: Option<AggrModifier> = None;
        if is_aggr {
            modifier = self.maybe_parse_aggr_modifier()?;
        }
        self.expect("(")?;
        let mut args: Vec<Expr> = Vec::new();
        if self.peek().raw != ")" {
            loop {
                args.push(self.parse_expr()?);
                match self.peek().raw.as_str() {
                    "," => {
                        self.advance();
                        // Trailing comma: `sum(x,)` is allowed.
                        if self.peek().raw == ")" {
                            break;
                        }
                    }
                    ")" => break,
                    other => {
                        return Err(ParseError {
                            message: format!("expected ',' or ')' in func args, got {other:?}"),
                            at: self.peek().start,
                        });
                    }
                }
            }
        }
        self.expect(")")?;
        let mut keep_metric_names = false;
        if self.peek().raw.eq_ignore_ascii_case("keep_metric_names") {
            keep_metric_names = true;
            self.advance();
        }
        // Trailing modifier form (`sum(x) by (l)`): only parse
        // if we didn't already grab one in the leading position.
        if modifier.is_none() {
            modifier = self.maybe_parse_aggr_modifier()?;
        }
        // Aggregate `limit N` — caps result series count.
        let limit = self.maybe_parse_limit()?;
        Ok(Expr::Func(FuncExpr {
            name,
            args,
            keep_metric_names,
            modifier,
            limit,
        }))
    }

    fn maybe_parse_limit(&mut self) -> Result<Option<u64>, ParseError> {
        if !self.peek().raw.eq_ignore_ascii_case("limit") {
            return Ok(None);
        }
        self.advance();
        let n_tok = self.advance();
        let n: u64 = n_tok.raw.parse().map_err(|_| ParseError {
            message: format!("expected number after `limit`, got {:?}", n_tok.raw),
            at: n_tok.start,
        })?;
        Ok(Some(n))
    }

    fn maybe_parse_aggr_modifier(&mut self) -> Result<Option<AggrModifier>, ParseError> {
        let op = match self.peek().raw.to_ascii_lowercase().as_str() {
            "by" => AggrModifierOp::By,
            "without" => AggrModifierOp::Without,
            _ => return Ok(None),
        };
        self.advance();
        self.expect("(")?;
        let mut labels: Vec<String> = Vec::new();
        if self.peek().raw != ")" {
            loop {
                let t = self.advance();
                if t.is_eof() {
                    return Err(ParseError {
                        message: "unexpected EOF in aggr modifier".into(),
                        at: t.start,
                    });
                }
                let label = if is_string_token(&t) {
                    unquote_string(&t.raw)?
                } else {
                    unescape_ident(&t.raw)
                };
                labels.push(label);
                match self.peek().raw.as_str() {
                    "," => {
                        self.advance();
                        // Trailing comma: `by (a, b,)` is allowed.
                        if self.peek().raw == ")" {
                            break;
                        }
                    }
                    ")" => break,
                    other => {
                        return Err(ParseError {
                            message: format!("expected ',' or ')' in aggr modifier, got {other:?}"),
                            at: self.peek().start,
                        });
                    }
                }
            }
        }
        self.expect(")")?;
        Ok(Some(AggrModifier { op, args: labels }))
    }

    fn parse_at_expr(&mut self) -> Result<Expr, ParseError> {
        if self.peek().raw != "@" {
            return Err(ParseError {
                message: format!("expected `@`, got {:?}", self.peek().raw),
                at: self.peek().start,
            });
        }
        self.advance();
        // `@ <expr>` accepts any single expression sans
        // rollup suffix. Numbers, identifiers, function
        // calls, parens are all valid.
        self.parse_single_expr_without_rollup_suffix()
    }

    /// `[<metric_name>] [ '{' label_filters '}' ]`
    fn parse_metric_expr(&mut self) -> Result<Expr, ParseError> {
        let mut filters: Vec<LabelFilter> = Vec::new();
        let mut have_metric_name = false;

        // Optional leading metric name. Unescape per upstream
        // so the stored value carries the canonical form
        // (e.g. `foo\ bar` → `foo bar`); the prettifier
        // re-escapes when serialising.
        if is_ident_token(self.peek()) {
            let name_tok = self.advance();
            filters.push(LabelFilter {
                label: "__name__".into(),
                op: LabelFilterOp::Eq,
                value: unescape_ident(&name_tok.raw),
                is_template_ref: false,
                value_expr: None,
                was_quoted: false,
            });
            have_metric_name = true;
        }

        // Optional `{ ... }` block. May contain one or more
        // comma-separated label filters, with multiple filter
        // GROUPS separated by the keyword `or`. Each group
        // gets its own row in `label_filterss`; if the metric
        // expr has a leading metric name, that filter is
        // duplicated into every group's [0] slot so the
        // prettifier can recover it via getMetricName.
        let mut groups: Vec<Vec<LabelFilter>> = Vec::new();
        if self.peek().raw == "{" {
            self.advance();
            if self.peek().raw == "}" {
                self.advance();
                if !have_metric_name {
                    return Ok(Expr::Metric(MetricExpr::default()));
                }
                return Ok(Expr::Metric(MetricExpr {
                    label_filterss: vec![filters],
                }));
            }
            // First group — append to the seed `filters`
            // (which may already hold the leading metric name).
            self.parse_label_filter_group_into(&mut filters)?;
            groups.push(filters.clone());
            // The first group may have picked up an inline
            // metric-name filter via the string-as-name
            // shortcut (`{"metric", ...}`). Recognise that and
            // treat it the same as a leading bare-ident
            // metric name for the purposes of replicating
            // into subsequent `or` groups.
            let first_metric_filter: Option<LabelFilter> = groups[0]
                .iter()
                .find(|lf| lf.label == "__name__" && matches!(lf.op, LabelFilterOp::Eq))
                .cloned();
            // Subsequent `or`-prefixed groups.
            while self.peek().raw.eq_ignore_ascii_case("or") {
                self.advance();
                let mut next_group: Vec<LabelFilter> = Vec::new();
                self.parse_label_filter_group_into(&mut next_group)?;
                // Replicate the leading metric-name filter
                // ONLY when the just-parsed group doesn't
                // declare its own `__name__` filter. This
                // preserves the upstream invariant ("every
                // group's lf[0] is the metric-name filter")
                // while leaving disagreeing-name groups
                // (`{__name__="foo" or __name__="bar"}`)
                // untouched so the prettifier can detect the
                // mixed case and decline the bare-name form.
                if let Some(name_filter) = &first_metric_filter {
                    let has_name = next_group.iter().any(|lf| lf.label == "__name__");
                    if !has_name {
                        next_group.insert(0, name_filter.clone());
                    }
                }
                groups.push(next_group);
            }
            if self.peek().raw != "}" {
                return Err(ParseError {
                    message: format!(
                        "expected '}}' or 'or' in label-filter list, got {:?}",
                        self.peek().raw
                    ),
                    at: self.peek().start,
                });
            }
            self.advance();
        }

        if groups.is_empty() {
            if filters.is_empty() {
                return Err(ParseError {
                    message: "expected metric expression".into(),
                    at: self.peek().start,
                });
            }
            // Bare metric name with no `{}` block.
            return Ok(Expr::Metric(MetricExpr {
                label_filterss: vec![filters],
            }));
        }

        // Apply `__name__` rewriting: an inline `__name__="x"`
        // filter with no preceding metric name acts like
        // a leading metric name in upstream's canonical form.
        let groups = canonicalize_metric_name_groups(groups);

        Ok(Expr::Metric(MetricExpr {
            label_filterss: groups,
        }))
    }

    /// Parse `lf[, lf]*` into `out`, terminating on `}` or
    /// `or` (case-insensitive). Accepts a leading string-only
    /// item as a metric-name shorthand: `{"metric", a="1"}` ≡
    /// `metric{a="1"}`. Each string-only item synthesises an
    /// `__name__="<string>"` filter.
    fn parse_label_filter_group_into(
        &mut self,
        out: &mut Vec<LabelFilter>,
    ) -> Result<(), ParseError> {
        loop {
            // Trailing comma in label list: `{a="b",}` is
            // valid upstream — terminate cleanly when the
            // current token is the close brace or `or`.
            match self.peek().raw.as_str() {
                "}" => return Ok(()),
                kw if kw.eq_ignore_ascii_case("or") => return Ok(()),
                _ => {}
            }
            // String-as-metric-name shortcut: a bare string
            // literal followed by `,` or `}` becomes
            // `__name__="<string>"`.
            let cur = self.peek().clone();
            if is_string_token(&cur) {
                let next = self.tokens.get(self.pos + 1).cloned().unwrap_or(Token {
                    raw: String::new(),
                    start: 0,
                });
                if next.raw == "," || next.raw == "}" || next.raw.eq_ignore_ascii_case("or") {
                    self.advance();
                    let value = unquote_string(&cur.raw)?;
                    out.push(LabelFilter {
                        label: "__name__".into(),
                        op: LabelFilterOp::Eq,
                        value,
                        is_template_ref: false,
                        value_expr: None,
                        // Metric-name-as-string slot uses the
                        // Prometheus 3.x quoted form;
                        // remember it for round-trip output.
                        was_quoted: true,
                    });
                    match self.peek().raw.as_str() {
                        "," => {
                            self.advance();
                            continue;
                        }
                        "}" => return Ok(()),
                        kw if kw.eq_ignore_ascii_case("or") => return Ok(()),
                        _ => {}
                    }
                    continue;
                }
            }
            let lf = self.parse_label_filter()?;
            out.push(lf);
            match self.peek().raw.as_str() {
                "," => {
                    self.advance();
                }
                "}" => return Ok(()),
                kw if kw.eq_ignore_ascii_case("or") => return Ok(()),
                other => {
                    return Err(ParseError {
                        message: format!(
                            "expected ',', '}}' or 'or' in label-filter list, got {:?}",
                            other
                        ),
                        at: self.peek().start,
                    });
                }
            }
        }
    }

    /// Parse the expression after a `prefix` keyword. Atoms
    /// are either string literals or bare idents (template
    /// refs); they may be chained with `+`. The chain stops at
    /// the first non-atom token so the binary-op loop in
    /// `parse_expr` still sees the right operand.
    fn parse_prefix_expr(&mut self) -> Result<Expr, ParseError> {
        let mut acc = self.parse_prefix_atom()?;
        loop {
            if self.peek().raw != "+" {
                break;
            }
            let next = self.tokens.get(self.pos + 1).cloned().unwrap_or(Token {
                raw: String::new(),
                start: 0,
            });
            if !is_prefix_atom_token(&next) {
                break;
            }
            self.advance(); // consume '+'
            let rhs = self.parse_prefix_atom()?;
            acc = Expr::Binary(BinaryOpExpr {
                op: BinaryOp::Add,
                left: Box::new(acc),
                right: Box::new(rhs),
                bool_modifier: false,
                group_modifier: None,
                join_modifier: None,
                join_modifier_prefix: None,
                keep_metric_names: false,
            });
        }
        Ok(acc)
    }

    fn parse_prefix_atom(&mut self) -> Result<Expr, ParseError> {
        let tok = self.advance();
        if is_string_token(&tok) {
            let single_quoted = tok.raw.starts_with('\'');
            Ok(Expr::String(StringExpr {
                value: unquote_string(&tok.raw)?,
                single_quoted,
            }))
        } else if is_ident_token(&tok) {
            Ok(Expr::Metric(MetricExpr {
                label_filterss: vec![vec![LabelFilter {
                    label: "__name__".into(),
                    op: LabelFilterOp::Eq,
                    value: unescape_ident(&tok.raw),
                    is_template_ref: false,
                    value_expr: None,
                    was_quoted: false,
                }]],
            }))
        } else {
            Err(ParseError {
                message: format!(
                    "expected string or ident in `prefix` expression, got {:?}",
                    tok.raw
                ),
                at: tok.start,
            })
        }
    }

    fn parse_label_filter(&mut self) -> Result<LabelFilter, ParseError> {
        let label_tok = self.advance();
        if label_tok.raw.is_empty() {
            return Err(ParseError {
                message: "unexpected EOF in label filter".into(),
                at: label_tok.start,
            });
        }
        let is_quoted_label = is_string_token(&label_tok);
        // Label name may be a quoted string (Prometheus
        // allows `{"label" = "value"}` for non-identifier-safe
        // names) or a bare identifier. Bare identifiers go
        // through the same unescape pass as metric names.
        let label = if is_quoted_label {
            unquote_string(&label_tok.raw)?
        } else {
            unescape_ident(&label_tok.raw)
        };
        // Bare ident followed by `,`, `}`, or `or` is a WITH
        // template reference — `m{x, y="z"}` expands to the
        // `x` binding's filters merged with `y="z"`. Quoted
        // labels can't be template refs (they require an op).
        if !is_quoted_label {
            let next = self.peek().raw.as_str();
            if next == "," || next == "}" || next.eq_ignore_ascii_case("or") {
                return Ok(LabelFilter {
                    label,
                    op: LabelFilterOp::Eq,
                    value: String::new(),
                    is_template_ref: true,
                    value_expr: None,
                    was_quoted: false,
                });
            }
        }
        let op_tok = self.advance();
        let op = match op_tok.raw.as_str() {
            "=" => LabelFilterOp::Eq,
            "!=" => LabelFilterOp::Ne,
            "=~" => LabelFilterOp::EqRegex,
            "!~" => LabelFilterOp::NeRegex,
            other => {
                return Err(ParseError {
                    message: format!("expected label-filter op, got {other:?}"),
                    at: op_tok.start,
                });
            }
        };
        // Label values may be a single string literal, a
        // chain of strings joined by `+`, or a mixed chain of
        // strings and bare-ident WITH-template refs
        // (`m{foo=x+"y"}`). Pure-string chains fold inline
        // into `value`; mixed chains land in `value_expr` and
        // resolve to a string after WITH expansion + constant
        // folding.
        let (value, value_expr) = self.parse_label_filter_value()?;
        Ok(LabelFilter {
            label,
            op,
            value,
            value_expr,
            is_template_ref: false,
            was_quoted: is_quoted_label,
        })
    }

    /// Parse `<atom>[ + <atom>]*` where each atom is either a
    /// string literal or a bare ident (treated as a WITH
    /// template ref). Returns `(value, value_expr)` — exactly
    /// one of the two is populated: pure-string chains fold
    /// into `value`, mixed chains land in `value_expr` for
    /// later expansion.
    fn parse_label_filter_value(&mut self) -> Result<(String, Option<Box<Expr>>), ParseError> {
        // Track all atoms; if any is an ident we'll need to
        // build a binary-op tree to defer to expansion time.
        let mut atoms: Vec<Expr> = Vec::new();
        atoms.push(self.parse_label_value_atom()?);
        while self.peek().raw == "+" {
            let next = self.tokens.get(self.pos + 1).cloned().unwrap_or(Token {
                raw: String::new(),
                start: 0,
            });
            if !is_label_value_atom_token(&next) {
                break;
            }
            self.advance(); // consume '+'
            atoms.push(self.parse_label_value_atom()?);
        }
        let all_strings = atoms.iter().all(|a| matches!(a, Expr::String(_)));
        if all_strings {
            let mut value = String::new();
            for a in atoms {
                if let Expr::String(s) = a {
                    value.push_str(&s.value);
                }
            }
            return Ok((value, None));
        }
        // Build a left-associative `+` chain — constant
        // folding will collapse adjacent strings later.
        let mut iter = atoms.into_iter();
        let mut acc = iter.next().expect("at least one atom");
        for rhs in iter {
            acc = Expr::Binary(BinaryOpExpr {
                op: BinaryOp::Add,
                left: Box::new(acc),
                right: Box::new(rhs),
                bool_modifier: false,
                group_modifier: None,
                join_modifier: None,
                join_modifier_prefix: None,
                keep_metric_names: false,
            });
        }
        Ok((String::new(), Some(Box::new(acc))))
    }

    fn parse_label_value_atom(&mut self) -> Result<Expr, ParseError> {
        let tok = self.advance();
        if is_string_token(&tok) {
            let single_quoted = tok.raw.starts_with('\'');
            return Ok(Expr::String(StringExpr {
                value: unquote_string(&tok.raw)?,
                single_quoted,
            }));
        }
        if is_ident_token(&tok) {
            return Ok(Expr::Metric(MetricExpr {
                label_filterss: vec![vec![LabelFilter {
                    label: "__name__".into(),
                    op: LabelFilterOp::Eq,
                    value: unescape_ident(&tok.raw),
                    is_template_ref: false,
                    was_quoted: false,
                    value_expr: None,
                }]],
            }));
        }
        Err(ParseError {
            message: format!("expected string or ident in label value, got {:?}", tok.raw),
            at: tok.start,
        })
    }
}

/// Names of all MetricsQL aggregate functions, mirroring
/// upstream's `aggrFuncs` map. Used during parsing to decide
/// whether `sum by (l) (...)` (modifier-before-args form) is
/// legal — only known aggregates can take that shape.
fn is_aggr_func(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "any"
            | "avg"
            | "bottomk"
            | "bottomk_avg"
            | "bottomk_max"
            | "bottomk_median"
            | "bottomk_last"
            | "bottomk_min"
            | "count"
            | "count_values"
            | "distinct"
            | "geomean"
            | "group"
            | "histogram"
            | "limitk"
            | "mad"
            | "max"
            | "median"
            | "min"
            | "mode"
            | "outliers_iqr"
            | "outliers_mad"
            | "outliersk"
            | "quantile"
            | "quantiles"
            | "share"
            | "stddev"
            | "stdvar"
            | "sum"
            | "sum2"
            | "topk"
            | "topk_avg"
            | "topk_max"
            | "topk_median"
            | "topk_last"
            | "topk_min"
            | "zscore"
    )
}

fn is_dollar_interval(raw: &str) -> bool {
    matches!(raw, "$__interval" | "$__rate_interval")
}

fn is_prefix_atom_token(t: &Token) -> bool {
    is_string_token(t) || is_ident_token(t)
}

/// Same atom shape as `is_prefix_atom_token` — strings or
/// bare idents. Used to detect mixed label-value chains like
/// `foo=x+"bar"` so the parser can defer the chain to a
/// `value_expr` AST until WITH expansion fills it in.
fn is_label_value_atom_token(t: &Token) -> bool {
    is_string_token(t) || is_ident_token(t)
}

fn is_ident_token(t: &Token) -> bool {
    if t.is_eof() {
        return false;
    }
    let first = t.raw.chars().next().unwrap_or('\0');
    // `:` is a valid first-ident-char in MetricsQL — that's
    // how stuff like `m_e:tri44:_c123` lex as one token.
    // Disambiguation against the rollup step marker happens
    // contextually inside `[ ... ]` (the rollup parser peeks
    // for a `:`-prefixed token explicitly).
    first.is_alphabetic() || first == '_' || first == ':' || first == '\\'
}

fn is_rollup_start(t: &Token) -> bool {
    if t.is_eof() {
        return false;
    }
    matches!(t.raw.as_str(), "[" | "@") || is_offset_keyword(&t.raw)
}

fn is_offset_keyword(raw: &str) -> bool {
    raw.eq_ignore_ascii_case("offset")
}

fn is_number_prefix(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    if bytes[0].is_ascii_digit() {
        return true;
    }
    bytes[0] == b'.' && bytes.len() >= 2 && bytes[1].is_ascii_digit()
}

fn is_inf_or_nan(raw: &str) -> bool {
    raw.eq_ignore_ascii_case("inf") || raw.eq_ignore_ascii_case("nan")
}

/// True when the entire token is a duration literal (`5m`,
/// `1h30m`, `100ms`). Necessary for the dispatcher to route
/// duration-literal exprs to `Expr::Duration` rather than
/// number / identifier paths.
fn is_full_duration_literal(raw: &str) -> bool {
    if raw.is_empty() {
        return false;
    }
    // Re-run the lexer's duration scanner over this exact
    // token; consider it "full" only when the scan covers
    // the entire token.
    match crate::lexer::scan_duration_external(raw) {
        Some(end) => end == raw.len(),
        None => false,
    }
}

/// Decode a number literal token into its `f64` value. Honors
/// `0x`/`0o`/`0b` prefixes and the byte-multiplier suffixes
/// (`KB`, `MiB`, …) the lexer accepts. Mirrors upstream's
/// `parsePositiveNumber`.
fn parse_number_literal(s: &str) -> Result<f64, ParseError> {
    let lower = s.to_lowercase();
    if lower == "inf" {
        return Ok(f64::INFINITY);
    }
    if lower == "nan" {
        return Ok(f64::NAN);
    }
    // Special integer prefixes.
    if let Some(stripped) = lower.strip_prefix("0x") {
        return i64::from_str_radix(stripped, 16)
            .map(|v| v as f64)
            .map_err(|e| ParseError {
                message: format!("hex: {e}"),
                at: 0,
            });
    }
    if let Some(stripped) = lower.strip_prefix("0b") {
        return i64::from_str_radix(stripped, 2)
            .map(|v| v as f64)
            .map_err(|e| ParseError {
                message: format!("bin: {e}"),
                at: 0,
            });
    }
    if let Some(stripped) = lower.strip_prefix("0o") {
        return i64::from_str_radix(stripped, 8)
            .map(|v| v as f64)
            .map_err(|e| ParseError {
                message: format!("oct: {e}"),
                at: 0,
            });
    }
    // Byte-multiplier suffixes — upstream `parsePositiveNumber`.
    let mut mantissa = lower.as_str();
    let mut multiplier: f64 = 1.0;
    for (suffix, m) in &[
        ("kib", 1024_f64),
        ("ki", 1024_f64),
        ("kb", 1000_f64),
        ("k", 1000_f64),
        ("mib", 1024.0 * 1024.0),
        ("mi", 1024.0 * 1024.0),
        ("mb", 1000.0 * 1000.0),
        ("m", 1000.0 * 1000.0),
        ("gib", 1024.0_f64.powi(3)),
        ("gi", 1024.0_f64.powi(3)),
        ("gb", 1000.0_f64.powi(3)),
        ("g", 1000.0_f64.powi(3)),
        ("tib", 1024.0_f64.powi(4)),
        ("ti", 1024.0_f64.powi(4)),
        ("tb", 1000.0_f64.powi(4)),
        ("t", 1000.0_f64.powi(4)),
    ] {
        if let Some(stripped) = mantissa.strip_suffix(suffix) {
            mantissa = stripped;
            multiplier = *m;
            break;
        }
    }
    // Underscore digit separators (`1_000_000`, `1.5_e3`)
    // are accepted by the lexer's number scanner; strip them
    // for the float-parse layer.
    let cleaned: String = mantissa.chars().filter(|&c| c != '_').collect();
    let v: f64 = cleaned.parse().map_err(|e| ParseError {
        message: format!("number: {e}"),
        at: 0,
    })?;
    Ok(v * multiplier)
}

/// Parse a binary operator from the current token, accepting
/// both symbolic (`+`, `-`, `*`, …) and keyword (`and`, `or`,
/// `unless`, `if`, `ifnot`, `default`, `atan2`) forms.
/// Returns `None` if the token isn't a binary op.
fn parse_binary_op(raw: &str) -> Option<BinaryOp> {
    if let Some(op) = match raw {
        "+" => Some(BinaryOp::Add),
        "-" => Some(BinaryOp::Sub),
        "*" => Some(BinaryOp::Mul),
        "/" => Some(BinaryOp::Div),
        "%" => Some(BinaryOp::Mod),
        "^" => Some(BinaryOp::Pow),
        "==" => Some(BinaryOp::Eq),
        "!=" => Some(BinaryOp::Ne),
        "<" => Some(BinaryOp::Lt),
        "<=" => Some(BinaryOp::Le),
        ">" => Some(BinaryOp::Gt),
        ">=" => Some(BinaryOp::Ge),
        _ => None,
    } {
        return Some(op);
    }
    let lower = raw.to_ascii_lowercase();
    match lower.as_str() {
        "and" => Some(BinaryOp::And),
        "or" => Some(BinaryOp::Or),
        "unless" => Some(BinaryOp::Unless),
        "if" => Some(BinaryOp::If),
        "ifnot" => Some(BinaryOp::IfNot),
        "default" => Some(BinaryOp::Default),
        "atan2" => Some(BinaryOp::Atan2),
        _ => None,
    }
}

fn binary_op_priority(op: BinaryOp) -> i32 {
    use BinaryOp::*;
    match op {
        Default => -1,
        If | IfNot => 0,
        Or => 1,
        And | Unless => 2,
        Eq | Ne | Lt | Le | Gt | Ge => 3,
        Add | Sub => 4,
        Mul | Div | Mod | Atan2 => 5,
        Pow => 6,
    }
}

fn is_right_associative_binary_op(op: BinaryOp) -> bool {
    matches!(op, BinaryOp::Pow)
}

/// Rotate a freshly-built `BinaryOpExpr` so the operator
/// precedence ordering across nested binary ops is correct.
/// Mirrors upstream's `balanceBinaryOp` exactly.
fn balance_binary_op(be: BinaryOpExpr) -> Expr {
    // If left isn't a BinaryOp, no balance needed.
    let left_op = match &*be.left {
        Expr::Binary(b) => b.op,
        _ => return Expr::Binary(be),
    };
    let lp = binary_op_priority(left_op);
    let rp = binary_op_priority(be.op);
    if rp < lp {
        return Expr::Binary(be);
    }
    if rp == lp && !is_right_associative_binary_op(be.op) {
        return Expr::Binary(be);
    }
    // Rotate: take be.left, swap its right with the
    // re-balanced node.
    let Expr::Binary(mut bel) = *be.left else {
        unreachable!()
    };
    let new_right = balance_binary_op(BinaryOpExpr {
        op: be.op,
        left: bel.right,
        right: be.right,
        bool_modifier: be.bool_modifier,
        group_modifier: be.group_modifier,
        join_modifier: be.join_modifier,
        join_modifier_prefix: be.join_modifier_prefix,
        keep_metric_names: be.keep_metric_names,
    });
    bel.right = Box::new(new_right);
    Expr::Binary(bel)
}

/// Expand `with (...) body` template references. Walks the
/// tree and replaces FuncExpr or MetricExpr nodes whose name
/// matches a binding with the binding's expression (with
/// formal-arg substitution where applicable).
///
/// This is a simplified port of upstream's `expandWithExpr` —
/// it covers constant substitution, expression substitution,
/// parameterized function templates (`f(x) = x+1`), and inner
/// `with (...)` nesting. It does NOT yet implement label-
/// filter templates inside `{...}` (where a binding name
/// becomes a label-filter list).
fn expand_with_expr(was: &[WithArgExpr], e: Expr) -> Result<Expr, ParseError> {
    match e {
        Expr::With(w) => {
            // Append bindings as-is — DO NOT pre-expand each
            // binding's body. Upstream resolves references at
            // lookup time using the slice `was[..idx]` so that
            // each binding's body sees only the bindings
            // declared before it; re-expanding here would
            // double-substitute under nested templates.
            let mut combined: Vec<WithArgExpr> = was.to_vec();
            combined.extend(w.bindings);
            expand_with_expr(&combined, *w.body)
        }
        Expr::Binary(mut b) => {
            let l = std::mem::replace(
                &mut *b.left,
                Expr::Number(NumberExpr {
                    value: 0.0,
                    literal: "0".into(),
                }),
            );
            let r = std::mem::replace(
                &mut *b.right,
                Expr::Number(NumberExpr {
                    value: 0.0,
                    literal: "0".into(),
                }),
            );
            b.left = Box::new(expand_with_expr(was, l)?);
            b.right = Box::new(expand_with_expr(was, r)?);
            if let Some(g) = b.group_modifier.as_mut() {
                g.labels = expand_modifier_args(was, &g.labels);
            }
            if let Some(j) = b.join_modifier.as_mut() {
                j.labels = expand_modifier_args(was, &j.labels);
            }
            if let Some(p) = b.join_modifier_prefix.take() {
                b.join_modifier_prefix = Some(Box::new(expand_with_expr(was, *p)?));
            }
            Ok(Expr::Binary(b))
        }
        Expr::Func(mut f) => {
            // Expand each arg first.
            let new_args: Result<Vec<Expr>, ParseError> = f
                .args
                .into_iter()
                .map(|a| expand_with_expr(was, a))
                .collect();
            let mut new_args = new_args?;
            // If the function name matches a WITH binding,
            // substitute its body — with formal-arg replacement
            // when the binding is a function template.
            if let Some((idx, wa)) = lookup_with_arg(was, &f.name) {
                // `f((a, b))` — when a function template
                // expects N args but the call site passed a
                // single ParensExpr with N elements, unwrap.
                if !wa.args.is_empty()
                    && new_args.len() == 1
                    && matches!(&new_args[0], Expr::Paren(_))
                    && wa.args.len() != 1
                    && let Expr::Paren(p) = new_args.pop().unwrap()
                {
                    new_args = p.exprs;
                }
                // Pass `was[..idx]` so the binding can't see
                // itself or any later binding — prevents
                // cycles like `with (x = x+1) ...`.
                return expand_with_template(&was[..idx], wa.clone(), new_args);
            }
            // Aggregate `by/without` modifier label list — apply
            // formal-arg substitution to each label name.
            if let Some(m) = f.modifier.as_mut() {
                m.args = expand_modifier_args(was, &m.args);
            }
            f.args = new_args;
            Ok(Expr::Func(f))
        }
        Expr::Paren(mut p) => {
            let new_exprs: Result<Vec<Expr>, ParseError> = p
                .exprs
                .into_iter()
                .map(|x| expand_with_expr(was, x))
                .collect();
            p.exprs = new_exprs?;
            Ok(Expr::Paren(p))
        }
        Expr::Rollup(mut r) => {
            let inner = std::mem::replace(
                &mut *r.expr,
                Expr::Number(NumberExpr {
                    value: 0.0,
                    literal: "0".into(),
                }),
            );
            r.expr = Box::new(expand_with_expr(was, inner)?);
            r.window = r.window.map(|d| expand_duration(was, d));
            r.step = r.step.map(|d| expand_duration(was, d));
            r.offset = r.offset.map(|d| expand_duration(was, d));
            if let Some(at) = r.at.take() {
                r.at = Some(Box::new(expand_with_expr(was, *at)?));
            }
            Ok(Expr::Rollup(r))
        }
        Expr::Metric(m) => expand_metric_expr(was, m),
        other => Ok(other),
    }
}

/// Mirrors upstream `expandModifierArgs`. Each name in a
/// modifier label list (e.g. `sum(...) by (a, b)`) may match a
/// WITH binding whose value is a metric name or a parens
/// group of metric names; in that case the name is replaced
/// (or expanded into multiple names). Function-template
/// bindings are left alone. Duplicates are removed.
fn expand_modifier_args(was: &[WithArgExpr], args: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for arg in args {
        let Some((_, wa)) = lookup_with_arg(was, arg) else {
            out.push(arg.clone());
            continue;
        };
        if !wa.args.is_empty() {
            // Function templates can't appear in modifier
            // lists — keep the literal arg name.
            out.push(arg.clone());
            continue;
        }
        match &wa.expr {
            Expr::Metric(me) => {
                if let Some(name) = metric_only_name(me) {
                    out.push(name);
                } else {
                    out.push(arg.clone());
                }
            }
            Expr::Paren(p) => {
                for inner in &p.exprs {
                    if let Expr::Metric(me) = inner
                        && let Some(name) = metric_only_name(me)
                    {
                        out.push(name);
                    }
                }
            }
            _ => out.push(arg.clone()),
        }
    }
    // Dedup, preserving first occurrence.
    let mut seen: Vec<String> = Vec::with_capacity(out.len());
    for s in out {
        if !seen.contains(&s) {
            seen.push(s);
        }
    }
    seen
}

/// If a `MetricExpr` is exactly a single bare-name filter
/// (`metric` with no other label filters and no `or` groups),
/// return that name. Used by modifier-arg expansion to detect
/// when a WITH binding is a usable metric-name template.
fn metric_only_name(me: &MetricExpr) -> Option<String> {
    if me.label_filterss.len() != 1 {
        return None;
    }
    let g = &me.label_filterss[0];
    if g.len() != 1 {
        return None;
    }
    let lf = &g[0];
    if lf.label != "__name__" || !matches!(lf.op, LabelFilterOp::Eq) {
        return None;
    }
    Some(lf.value.clone())
}

/// Substitute a WITH-bound name appearing as a duration value.
/// `m[w]` parses with `w` stored as the window's text. If a
/// matching binding holds a literal duration or a number, swap
/// the value in. Otherwise leave the original text untouched
/// (the prettifier will round-trip it as-is and the case will
/// surface during evaluation, mirroring upstream behaviour).
fn expand_duration(was: &[WithArgExpr], d: DurationExpr) -> DurationExpr {
    let Some((idx, wa)) = lookup_with_arg(was, &d.value) else {
        return d;
    };
    if !wa.args.is_empty() {
        return d;
    }
    // Re-expand the binding under the prior scope so chained
    // duration aliases resolve (`with (w=5m, w2=w) m[w2]`).
    let expanded = expand_with_expr(&was[..idx], wa.expr.clone()).unwrap_or(wa.expr.clone());
    match expanded {
        Expr::Duration(d2) => d2,
        Expr::Number(n) => DurationExpr {
            value: if !n.literal.is_empty() {
                n.literal
            } else {
                format!("{}", n.value)
            },
            requires_step: false,
        },
        _ => d,
    }
}

/// Expand a `MetricExpr` under WITH bindings.
///
/// Steps:
///   1. Expand any template-ref filters inside each `or`
///      group — `m{x, y="z"}` where `x` is bound to
///      `{a="b"}` becomes `m{a="b", y="z"}`.
///   2. If the metric name itself is a binding, swap it in
///      and merge the remaining filters.
fn expand_metric_expr(was: &[WithArgExpr], m: MetricExpr) -> Result<Expr, ParseError> {
    let m = expand_label_filter_value_exprs(was, m)?;
    let m = expand_label_filter_template_refs(was, m)?;
    let Some(name) = first_group_metric_name(&m) else {
        return Ok(Expr::Metric(m));
    };
    let Some((idx, wa)) = lookup_with_arg(was, &name) else {
        return Ok(Expr::Metric(m));
    };
    // Function templates can't be referenced as a bare name —
    // upstream just keeps the original metric expr in that
    // case (so callers like `f(x)` parse normally).
    if !wa.args.is_empty() {
        return Ok(Expr::Metric(m));
    }
    let bound = expand_with_expr(&was[..idx], wa.expr.clone())?;
    // Pure bare-name swap (no extra filters or `or` groups in
    // the original): hand back the bound expression directly.
    let is_pure_bare = m.label_filterss.len() == 1 && m.label_filterss[0].len() == 1;
    if is_pure_bare {
        return Ok(bound);
    }
    // Otherwise, we need to merge our additional filters into
    // the bound expression. If the bound expanded to a Metric
    // we can do a clean merge; if it's a Rollup-wrapped Metric
    // we merge into the inner metric and re-wrap; otherwise
    // give up (matches upstream's "must be metric expr" error).
    match bound {
        Expr::Metric(bound_me) => Ok(Expr::Metric(merge_metric_filters(bound_me, m))),
        Expr::Rollup(mut r) => {
            if let Expr::Metric(bound_me) = *r.expr {
                r.expr = Box::new(Expr::Metric(merge_metric_filters(bound_me, m)));
                Ok(Expr::Rollup(r))
            } else {
                Err(ParseError {
                    message: format!(
                        "WITH binding {:?} must expand to a metric expr to be merged with extra filters",
                        name
                    ),
                    at: 0,
                })
            }
        }
        _ => Err(ParseError {
            message: format!(
                "WITH binding {:?} must expand to a metric expr to be merged with extra filters",
                name
            ),
            at: 0,
        }),
    }
}

/// Replace any `is_template_ref` filter inside the metric's
/// `or` groups with the filters from its WITH binding. A
/// template ref binding must expand to a MetricExpr; the
/// bound expression's `or` groups multiply against the
/// caller's other groups (cross-product).
/// Recurse into each label filter's deferred `value_expr`
/// (the parser stages mixed string-and-ident chains there)
/// and run WITH expansion. Once expanded the value either
/// becomes a constant string we can fold immediately, or
/// stays as an expression that simplify_constants will
/// collapse later.
fn expand_label_filter_value_exprs(
    was: &[WithArgExpr],
    mut m: MetricExpr,
) -> Result<MetricExpr, ParseError> {
    for group in m.label_filterss.iter_mut() {
        for lf in group.iter_mut() {
            if let Some(ve) = lf.value_expr.take() {
                let expanded = expand_with_expr(was, *ve)?;
                let folded = simplify_constants(expanded);
                if let Expr::String(s) = folded {
                    lf.value = s.value;
                } else {
                    return Err(ParseError {
                        message: format!(
                            "label value for {:?} did not reduce to a string after WITH expansion",
                            lf.label
                        ),
                        at: 0,
                    });
                }
            }
        }
    }
    Ok(m)
}

fn expand_label_filter_template_refs(
    was: &[WithArgExpr],
    m: MetricExpr,
) -> Result<MetricExpr, ParseError> {
    let mut out_groups: Vec<Vec<LabelFilter>> = Vec::with_capacity(m.label_filterss.len());
    for group in m.label_filterss.into_iter() {
        // For each filter, either keep it (non-template) or
        // expand to a list of `or` groups (each containing the
        // bound filters). The full group's expansion is the
        // cross-product across all template refs.
        let mut combined: Vec<Vec<LabelFilter>> = vec![Vec::new()];
        for lf in group.into_iter() {
            if !lf.is_template_ref {
                for g in combined.iter_mut() {
                    g.push(lf.clone());
                }
                continue;
            }
            let Some((idx, wa)) = lookup_with_arg(was, &lf.label) else {
                return Err(ParseError {
                    message: format!(
                        "WITH template {:?} not found inside label-filter list",
                        lf.label
                    ),
                    at: 0,
                });
            };
            if !wa.args.is_empty() {
                return Err(ParseError {
                    message: format!(
                        "WITH template {:?} cannot be used as a label filter — function templates are not supported here",
                        lf.label
                    ),
                    at: 0,
                });
            }
            let bound = expand_with_expr(&was[..idx], wa.expr.clone())?;
            let bound_groups = match bound {
                Expr::Metric(me) => me.label_filterss,
                _ => {
                    return Err(ParseError {
                        message: format!(
                            "WITH template {:?} must expand to a label-filter set",
                            lf.label
                        ),
                        at: 0,
                    });
                }
            };
            // Drop the binding's `__name__` filter — only
            // label filters get carried into the caller.
            let mut bound_groups: Vec<Vec<LabelFilter>> = bound_groups
                .into_iter()
                .map(|g| g.into_iter().filter(|f| f.label != "__name__").collect())
                .collect();
            // An empty `{}` binding has no groups; treat it as
            // a single empty group so the cross-product below
            // is the identity (`m{x}` with `x={}` → `m{}` →
            // `m`) instead of collapsing to zero groups.
            if bound_groups.is_empty() {
                bound_groups.push(Vec::new());
            }
            // Cross-product: each combined-so-far × each bound group.
            let mut next: Vec<Vec<LabelFilter>> = Vec::new();
            for c in &combined {
                for bg in &bound_groups {
                    let mut merged = c.clone();
                    merged.extend(bg.iter().cloned());
                    next.push(merged);
                }
            }
            combined = next;
        }
        out_groups.extend(combined);
    }
    let mut me = MetricExpr {
        label_filterss: out_groups,
    };
    me.label_filterss = canonicalize_metric_name_groups(me.label_filterss);
    Ok(me)
}

/// True only when the metric expr has a single filter group
/// and that group's first filter is `__name__="X"`. Returns X.
/// Mirrors the canonicalisation done at parse time.
fn first_group_metric_name(m: &MetricExpr) -> Option<String> {
    let g = m.label_filterss.first()?;
    let lf = g.first()?;
    if lf.label != "__name__" || !matches!(lf.op, LabelFilterOp::Eq) {
        return None;
    }
    Some(lf.value.clone())
}

/// Merge the bound MetricExpr's `or` groups with the caller's
/// extra filters. Each bound group is concatenated with each
/// caller group (cross-product) — matching upstream's WITH
/// expansion semantics for templates with `or`.
fn merge_metric_filters(bound: MetricExpr, mut caller: MetricExpr) -> MetricExpr {
    // Drop the leading `__name__` from caller (we're replacing
    // it with the bound expr's name(s)).
    for g in caller.label_filterss.iter_mut() {
        if let Some(idx) = g
            .iter()
            .position(|lf| lf.label == "__name__" && matches!(lf.op, LabelFilterOp::Eq))
        {
            g.remove(idx);
        }
    }
    let mut out: Vec<Vec<LabelFilter>> = Vec::new();
    for bg in &bound.label_filterss {
        for cg in &caller.label_filterss {
            let mut combined = bg.clone();
            combined.extend(cg.iter().cloned());
            out.push(combined);
        }
    }
    if out.is_empty() {
        // Caller had no groups (shouldn't happen — at least
        // one filter group always exists). Fall back to bound.
        return bound;
    }
    let mut me = MetricExpr {
        label_filterss: out,
    };
    // Re-run canonicalization so the merged groups carry the
    // same shape the parser produces (name at position 0,
    // duplicates removed).
    me.label_filterss = canonicalize_metric_name_groups(me.label_filterss);
    me
}

fn lookup_with_arg<'a>(was: &'a [WithArgExpr], name: &str) -> Option<(usize, &'a WithArgExpr)> {
    // Innermost binding wins — search from the end and report
    // the index so the caller can slice the scope to exclude
    // the matched binding (and anything defined after it).
    was.iter().enumerate().rev().find(|(_, wa)| wa.name == name)
}

/// Substitute a WITH binding's body, replacing formal args
/// with the actual arg expressions when the binding is a
/// function template (`f(x) = x+1` invoked as `f(2)` →
/// substitute `x` with `2` in the body). The caller MUST pass
/// `was[..idx]` (the scope BEFORE the binding) to prevent
/// recursive self-reference cycles.
fn expand_with_template(
    was_before: &[WithArgExpr],
    wa: WithArgExpr,
    args: Vec<Expr>,
) -> Result<Expr, ParseError> {
    if wa.args.len() != args.len() {
        return Err(ParseError {
            message: format!(
                "WITH template {:?} expects {} args, got {}",
                wa.name,
                wa.args.len(),
                args.len()
            ),
            at: 0,
        });
    }
    if wa.args.is_empty() {
        return expand_with_expr(was_before, wa.expr);
    }
    let mut combined: Vec<WithArgExpr> = was_before.to_vec();
    for (formal, actual) in wa.args.iter().zip(args.into_iter()) {
        combined.push(WithArgExpr {
            name: formal.clone(),
            args: Vec::new(),
            expr: actual,
        });
    }
    expand_with_expr(&combined, wa.expr)
}

/// Mirrors upstream `removeParensExpr`. Walks the tree and
/// replaces single-element `(expr)` groups with the inner
/// expression directly. Multi-element groups become anonymous
/// `union()` calls (FuncExpr with empty name).
fn remove_parens_expr(e: Expr) -> Expr {
    match e {
        Expr::Rollup(mut r) => {
            let inner = std::mem::replace(
                &mut *r.expr,
                Expr::Number(NumberExpr {
                    value: 0.0,
                    literal: "0".into(),
                }),
            );
            r.expr = Box::new(remove_parens_expr(inner));
            if let Some(at) = r.at.take() {
                r.at = Some(Box::new(remove_parens_expr(*at)));
            }
            Expr::Rollup(r)
        }
        Expr::Binary(mut b) => {
            let l = std::mem::replace(
                &mut *b.left,
                Expr::Number(NumberExpr {
                    value: 0.0,
                    literal: "0".into(),
                }),
            );
            let r = std::mem::replace(
                &mut *b.right,
                Expr::Number(NumberExpr {
                    value: 0.0,
                    literal: "0".into(),
                }),
            );
            b.left = Box::new(remove_parens_expr(l));
            b.right = Box::new(remove_parens_expr(r));
            Expr::Binary(b)
        }
        Expr::Func(mut f) => {
            f.args = f.args.into_iter().map(remove_parens_expr).collect();
            Expr::Func(f)
        }
        Expr::Paren(p) => {
            let mut args: Vec<Expr> = p.exprs.into_iter().map(remove_parens_expr).collect();
            if args.len() == 1 {
                return args.pop().unwrap();
            }
            // Multi-element parens become `union(...)` calls.
            Expr::Func(FuncExpr {
                name: String::new(),
                args,
                keep_metric_names: false,
                modifier: None,
                limit: None,
            })
        }
        Expr::With(mut w) => {
            for b in &mut w.bindings {
                let inner = std::mem::replace(
                    &mut b.expr,
                    Expr::Number(NumberExpr {
                        value: 0.0,
                        literal: "0".into(),
                    }),
                );
                b.expr = remove_parens_expr(inner);
            }
            let body = std::mem::replace(
                &mut *w.body,
                Expr::Number(NumberExpr {
                    value: 0.0,
                    literal: "0".into(),
                }),
            );
            w.body = Box::new(remove_parens_expr(body));
            Expr::With(w)
        }
        other => other,
    }
}

/// Post-pass that mirrors upstream `simplifyConstants`.
/// Walks the tree bottom-up and folds binary-op nodes whose
/// operands are both literals into a single literal. Run after
/// the full tree is built so balancing has already arranged
/// nodes by precedence.
fn simplify_constants(e: Expr) -> Expr {
    match e {
        Expr::Rollup(mut r) => {
            let inner = std::mem::replace(
                &mut *r.expr,
                Expr::Number(NumberExpr {
                    value: 0.0,
                    literal: "0".into(),
                }),
            );
            r.expr = Box::new(simplify_constants(inner));
            if let Some(at) = r.at.take() {
                r.at = Some(Box::new(simplify_constants(*at)));
            }
            Expr::Rollup(r)
        }
        Expr::Func(mut f) => {
            f.args = f.args.into_iter().map(simplify_constants).collect();
            Expr::Func(f)
        }
        Expr::Paren(mut p) => {
            p.exprs = p.exprs.into_iter().map(simplify_constants).collect();
            Expr::Paren(p)
        }
        Expr::Binary(be) => simplify_binary(be),
        other => other,
    }
}

fn simplify_binary(mut be: BinaryOpExpr) -> Expr {
    let left = simplify_constants(*be.left);
    let right = simplify_constants(*be.right);
    if let Some(p) = be.join_modifier_prefix.take() {
        be.join_modifier_prefix = Some(Box::new(simplify_constants(*p)));
    }
    // Both numbers: fold to a single number.
    if let (Expr::Number(l), Expr::Number(r)) = (&left, &right) {
        let n = binary_op_eval_number(be.op, l.value, r.value, be.bool_modifier);
        return Expr::Number(NumberExpr {
            value: n,
            literal: String::new(),
        });
    }
    // Both strings: handle `+` concat and comparisons.
    if let (Expr::String(l), Expr::String(r)) = (&left, &right) {
        if matches!(be.op, BinaryOp::Add) {
            return Expr::String(StringExpr {
                value: format!("{}{}", l.value, r.value),
                single_quoted: false,
            });
        }
        if is_cmp_op(be.op) {
            let ok = match be.op {
                BinaryOp::Eq => l.value == r.value,
                BinaryOp::Ne => l.value != r.value,
                BinaryOp::Gt => l.value > r.value,
                BinaryOp::Lt => l.value < r.value,
                BinaryOp::Ge => l.value >= r.value,
                BinaryOp::Le => l.value <= r.value,
                _ => unreachable!(),
            };
            let mut n = if ok { 1.0 } else { 0.0 };
            if !be.bool_modifier && n == 0.0 {
                n = f64::NAN;
            }
            return Expr::Number(NumberExpr {
                value: n,
                literal: String::new(),
            });
        }
        // Fall through — non-comparison op on two strings is a
        // type error at eval time, but the parser keeps the
        // expression as-is.
    }
    be.left = Box::new(left);
    be.right = Box::new(right);
    Expr::Binary(be)
}

fn is_cmp_op(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
    )
}

/// Mirrors upstream `binaryOpEvalNumber`. Folds two number
/// literals across a binary op into a single f64.
fn binary_op_eval_number(op: BinaryOp, left: f64, right: f64, is_bool: bool) -> f64 {
    use BinaryOp::*;
    if is_cmp_op(op) {
        let cmp = match op {
            Eq => bin_eq(left, right),
            Ne => bin_neq(left, right),
            Gt => left > right,
            Lt => left < right,
            Ge => left >= right,
            Le => left <= right,
            _ => unreachable!(),
        };
        if is_bool {
            return if cmp { 1.0 } else { 0.0 };
        }
        return if cmp { left } else { f64::NAN };
    }
    match op {
        Add => left + right,
        Sub => left - right,
        Mul => left * right,
        Div => left / right,
        Mod => left % right,
        Atan2 => left.atan2(right),
        Pow => {
            if left.is_nan() {
                f64::NAN
            } else {
                left.powf(right)
            }
        }
        And => {
            if left.is_nan() || right.is_nan() {
                f64::NAN
            } else {
                left
            }
        }
        Or => {
            if !left.is_nan() {
                left
            } else {
                right
            }
        }
        Unless => f64::NAN,
        Default => {
            if left.is_nan() {
                right
            } else {
                left
            }
        }
        If => {
            if right.is_nan() {
                f64::NAN
            } else {
                left
            }
        }
        IfNot => {
            if right.is_nan() {
                left
            } else {
                f64::NAN
            }
        }
        _ => unreachable!(),
    }
}

fn bin_eq(l: f64, r: f64) -> bool {
    if l.is_nan() {
        return r.is_nan();
    }
    l == r
}

fn bin_neq(l: f64, r: f64) -> bool {
    if l.is_nan() {
        return !r.is_nan();
    }
    if r.is_nan() {
        return true;
    }
    l != r
}

/// Canonicalize each label-filter group:
///   - move any `__name__="X"` filter to position 0
///   - drop exact-duplicate filters within the group
///
/// Mirrors upstream's `prependMetricNameFilter` +
/// `removeDuplicateLabelFilters` passes.
fn canonicalize_metric_name_groups(groups: Vec<Vec<LabelFilter>>) -> Vec<Vec<LabelFilter>> {
    groups
        .into_iter()
        .map(|group| {
            let mut g: Vec<LabelFilter> = group;
            let name_idx = g
                .iter()
                .position(|lf| lf.label == "__name__" && matches!(lf.op, LabelFilterOp::Eq));
            if let Some(i) = name_idx
                && i > 0
            {
                let name = g.remove(i);
                g.insert(0, name);
            }
            // Dedup — preserve first occurrence's order.
            // Compare on (label, op, value) only; the auxiliary
            // round-trip flags (`was_quoted`, `is_template_ref`)
            // shouldn't keep two semantically identical filters
            // separate.
            let mut seen: Vec<LabelFilter> = Vec::with_capacity(g.len());
            for lf in g {
                let dup = seen
                    .iter()
                    .any(|s| s.label == lf.label && s.op == lf.op && s.value == lf.value);
                if !dup {
                    seen.push(lf);
                }
            }
            seen
        })
        .collect()
}

fn is_string_token(t: &Token) -> bool {
    let bytes = t.raw.as_bytes();
    !bytes.is_empty() && matches!(bytes[0], b'"' | b'\'' | b'`')
}

/// Decode `\xHH`, `\uHHHH`, and printable-rune `\<r>`
/// escape sequences inside an identifier token. Mirrors
/// upstream `unescapeIdent` so the parser stores the
/// canonical (unescaped) form; the prettifier re-escapes on
/// output.
fn unescape_ident(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(next) = chars.next() else {
            out.push('\\');
            break;
        };
        if next == 'x' || next == 'X' {
            let h1 = chars.next().and_then(|c| c.to_digit(16));
            let h2 = chars.next().and_then(|c| c.to_digit(16));
            if let (Some(a), Some(b)) = (h1, h2) {
                let v = (a << 4) | b;
                if let Some(c) = char::from_u32(v) {
                    out.push(c);
                    continue;
                }
            }
            out.push('\\');
            out.push(next);
            continue;
        }
        if next == 'u' || next == 'U' {
            let mut v = 0u32;
            let mut ok = true;
            for _ in 0..4 {
                match chars.next().and_then(|c| c.to_digit(16)) {
                    Some(d) => v = (v << 4) | d,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok && let Some(c) = char::from_u32(v) {
                out.push(c);
                continue;
            }
            out.push('\\');
            out.push(next);
            continue;
        }
        // Printable-rune escape: `\<r>` decodes to the rune.
        // Non-printable / control runes preserve the
        // backslash so a downstream re-encode is unambiguous.
        if !next.is_control() {
            out.push(next);
        } else {
            out.push('\\');
            out.push(next);
        }
    }
    out
}

/// Strip the surrounding quote and decode escape sequences in
/// a string literal. Mirrors upstream `extractStringValue` for
/// the simple double-quoted case; the long tail (raw strings,
/// concatenated literals) lands in later passes.
pub(crate) fn unquote_string(raw: &str) -> Result<String, ParseError> {
    if raw.len() < 2 {
        return Err(ParseError {
            message: format!("malformed string literal {raw:?}"),
            at: 0,
        });
    }
    let bytes = raw.as_bytes();
    let quote = bytes[0];
    if bytes[bytes.len() - 1] != quote {
        return Err(ParseError {
            message: format!("string literal {raw:?} not properly closed"),
            at: 0,
        });
    }
    let inner = &raw[1..raw.len() - 1];
    if quote == b'`' {
        // Raw string — no escape processing.
        return Ok(inner.to_string());
    }
    // Interpreted string. Decode `\n`, `\"`, `\\`, `\xHH`,
    // `\uHHHH` etc.
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(next) = chars.next() else {
            out.push('\\');
            break;
        };
        match next {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            '0' => out.push('\0'),
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            '\'' => out.push('\''),
            '`' => out.push('`'),
            'x' => {
                let h1 = chars.next().and_then(|c| c.to_digit(16));
                let h2 = chars.next().and_then(|c| c.to_digit(16));
                if let (Some(a), Some(b)) = (h1, h2) {
                    let v = (a << 4) | b;
                    if let Some(c) = char::from_u32(v) {
                        out.push(c);
                    }
                }
            }
            'u' => {
                let mut v: u32 = 0;
                for _ in 0..4 {
                    let h = chars.next().and_then(|c| c.to_digit(16));
                    if let Some(d) = h {
                        v = (v << 4) | d;
                    }
                }
                if let Some(c) = char::from_u32(v) {
                    out.push(c);
                }
            }
            // Anything else: passthrough the backslash + char.
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_selector() {
        let e = parse("{}").unwrap();
        match e {
            Expr::Metric(me) => assert!(me.label_filterss.is_empty()),
            _ => panic!("expected MetricExpr"),
        }
    }

    #[test]
    fn bare_metric_name() {
        let e = parse("metric").unwrap();
        match e {
            Expr::Metric(me) => {
                assert_eq!(me.label_filterss.len(), 1);
                assert_eq!(me.label_filterss[0].len(), 1);
                let lf = &me.label_filterss[0][0];
                assert_eq!(lf.label, "__name__");
                assert_eq!(lf.value, "metric");
            }
            _ => panic!("expected MetricExpr"),
        }
    }

    #[test]
    fn metric_with_filter() {
        let e = parse(r#"metric{foo="bar"}"#).unwrap();
        match e {
            Expr::Metric(me) => {
                let lfs = &me.label_filterss[0];
                assert_eq!(lfs.len(), 2);
                assert_eq!(lfs[0].label, "__name__");
                assert_eq!(lfs[0].value, "metric");
                assert_eq!(lfs[1].label, "foo");
                assert_eq!(lfs[1].value, "bar");
            }
            _ => panic!("expected MetricExpr"),
        }
    }

    #[test]
    fn multi_filter() {
        let e = parse(r#"{a="1",b!="2",c=~"3",d!~"4"}"#).unwrap();
        match e {
            Expr::Metric(me) => {
                let lfs = &me.label_filterss[0];
                assert_eq!(lfs.len(), 4);
                assert_eq!(lfs[0].op, LabelFilterOp::Eq);
                assert_eq!(lfs[1].op, LabelFilterOp::Ne);
                assert_eq!(lfs[2].op, LabelFilterOp::EqRegex);
                assert_eq!(lfs[3].op, LabelFilterOp::NeRegex);
            }
            _ => panic!("expected MetricExpr"),
        }
    }
}
