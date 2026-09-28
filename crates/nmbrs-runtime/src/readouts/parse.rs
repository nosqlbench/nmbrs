// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Body-grammar parser for the workload `readouts:` block.
//! See SRD-63 §5.1.
//!
//! Push 3's grammar:
//!
//! - **Items** are whitespace-separated. Each item is one
//!   of:
//!   1. **Readout call** — bare lower-snake-case
//!      identifier matching a registered readout name.
//!      Followed (optionally) by space-separated
//!      `key=value` options up to the next non-`key=`
//!      token.
//!   2. **Parameterised form** — `name:arg` is sugar for
//!      `name pattern=arg` (the readout's primary option,
//!      currently the `metric:pattern` shorthand —
//!      generalised in Push 4).
//!   3. **Quoted literal** — `"text"` or `'text'`.
//!      Preserved verbatim.
//!   4. **Bare punctuation** — a non-identifier byte at an
//!      item boundary becomes a single-character literal.
//!
//! Push 3 punts:
//! - Color directives (`@RED`, `[#hex]`) — Push 4.
//! - Wildcard event-slot bindings — Push 4.
//! - Composition / override resolver (`+`-prefix) — Push 4.

use std::str::Chars;

use super::binder::{BakedBody, LayoutMode, RenderStep};
use super::color::ColorSpec;
use super::readout::{Lod, OptionValue, ReadoutOptions};
use super::registry::Registry;

/// Parse a body string into a [`BakedBody`].
///
/// Returns a list of warnings alongside the result so the
/// workload-load step can promote them to errors under
/// strict mode (per SRD-15).
pub fn bake(body: &str) -> Result<(BakedBody, Vec<String>), String> {
    let mut lex = Lexer::new(body);
    let mut steps: Vec<RenderStep> = Vec::new();
    let warnings: Vec<String> = Vec::new();
    let mut first_item = true;

    while let Some(token) = lex.next_token()? {
        // Color / style directives are zero-width — they
        // wrap the *next* item, so they don't get a
        // joining space and don't reset `first_item`.
        if let Token::ColorDirective(spec) = token {
            steps.push(RenderStep::ColorDirective(spec));
            continue;
        }
        // Inter-item joining whitespace renders as a single
        // space between item outputs. Skipped on the first
        // item.
        if !first_item {
            steps.push(RenderStep::Literal(" ".to_string()));
        }
        first_item = false;

        match token {
            Token::ColorDirective(_) => unreachable!(),
            Token::Quoted(s) => {
                steps.push(RenderStep::Literal(s));
            }
            Token::Punct(c) => {
                steps.push(RenderStep::Literal(c.to_string()));
            }
            Token::Ident(name) => {
                // Look ahead for `key=value` options. They
                // belong to this readout call; the next
                // non-key=value token starts a fresh item.
                let mut options = ReadoutOptions::new();
                let mut lod = Lod::default();
                let mut layout = LayoutMode::Auto;
                let mut color: Option<ColorSpec> = None;
                let mut primary_arg: Option<String> = None;

                // `name:arg` form — sugar for the primary
                // option. Detected by the lexer pre-pulling
                // the colon and value when it saw them
                // attached to the identifier.
                if let Some(arg) = lex.consume_attached_colon_arg() {
                    primary_arg = Some(arg);
                }

                while lex.peek_option().is_some() {
                    let (key, value) = lex.consume_option_pair()?;
                    apply_option(
                        &key,
                        value,
                        &mut lod,
                        &mut layout,
                        &mut color,
                        &mut options,
                        &mut primary_arg,
                    )?;
                }

                let readout = Registry::lookup(&name).ok_or_else(|| {
                    let known = Registry::all_names().join(", ");
                    format!("readouts: unknown readout name '{name}'. Known: {known}")
                })?;
                // Push 9b: the `name:arg` colon-shorthand
                // routes its argument into the option store
                // under the conventional key `pattern` —
                // that's what every readout's primary
                // option currently uses (matching the
                // SRD-63 §5.1 example
                // `metric:recall* ≡ metric pattern="recall*"`).
                if let Some(arg) = primary_arg {
                    options.set("pattern", OptionValue::Str(arg));
                }
                steps.push(RenderStep::Render {
                    readout,
                    lod,
                    layout,
                    options,
                    color,
                });
            }
        }
    }

    Ok((BakedBody::from_steps(steps), warnings))
}

fn apply_option(
    key: &str,
    value: OptionValue,
    lod: &mut Lod,
    layout: &mut LayoutMode,
    color: &mut Option<ColorSpec>,
    options: &mut ReadoutOptions,
    _primary_arg: &mut Option<String>,
) -> Result<(), String> {
    match key {
        "lod" => {
            *lod = parse_lod(&value).map_err(|e| format!("readouts: lod=…: {e}"))?;
        }
        "layout" => {
            *layout = parse_layout(&value).map_err(|e| format!("readouts: layout=…: {e}"))?;
        }
        "color" | "style" => {
            // `color=` and `style=` are aliases — both
            // route through the same parser. The
            // distinction in §5.2 is editorial only:
            // `style=ERROR` reads better than `color=ERROR`,
            // but both produce an `ColorSpec::Style(...)`
            // entry that resolves through the active
            // palette at render time.
            let token = match value {
                OptionValue::Str(s) => s,
                other => {
                    return Err(format!(
                        "readouts: {key}= must be a string token (RED, BRIGHT_GREEN, \
                     #aabbcc, ERROR, …); got {other:?}"
                    ));
                }
            };
            *color = Some(ColorSpec::parse(&token).ok_or_else(|| {
                format!(
                    "readouts: {key}=…: unknown colour / style '{token}' \
                     (RED|BRIGHT_RED|#rrggbb|ERROR|INFO|…)"
                )
            })?);
        }
        // Push 9b: every other key falls through to the
        // option store. Readouts read their own
        // domain-specific options (precision=, unit=,
        // pattern=, …) via `ReadoutOptions::get_*`.
        // Unknown keys aren't an error — readouts decide
        // which keys they care about — so an option that
        // no readout consumes is just ignored.
        other => {
            options.set(other, value);
        }
    }
    Ok(())
}

fn parse_lod(value: &OptionValue) -> Result<Lod, String> {
    match value {
        OptionValue::Str(s) => match s.as_str() {
            "compact" | "1" => Ok(Lod::Compact),
            "labeled" | "2" => Ok(Lod::Labeled),
            "expanded" | "3" => Ok(Lod::Expanded),
            other => Err(format!("unknown LOD '{other}' (compact/labeled/expanded)")),
        },
        OptionValue::Int(1) => Ok(Lod::Compact),
        OptionValue::Int(2) => Ok(Lod::Labeled),
        OptionValue::Int(3) => Ok(Lod::Expanded),
        other => Err(format!("LOD must be name or 1..=3, got {other:?}")),
    }
}

fn parse_layout(value: &OptionValue) -> Result<LayoutMode, String> {
    match value {
        OptionValue::Str(s) => match s.as_str() {
            "auto" => Ok(LayoutMode::Auto),
            "inline" => Ok(LayoutMode::Inline),
            "block" => Ok(LayoutMode::Block),
            other => Err(format!("unknown layout '{other}' (auto/inline/block)")),
        },
        other => Err(format!("layout must be a string, got {other:?}")),
    }
}

// ── Lexer ───────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum Token {
    Ident(String),
    Quoted(String),
    Punct(char),
    /// `@RED` / `@INFO` / `@#aabbcc` / `[#aabbcc]`. Single-
    /// shot inline color directive — wraps the next non-
    /// directive item.
    ColorDirective(ColorSpec),
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    /// `name:arg` attaches the colon and arg to the
    /// identifier; the lexer parks that arg here so the
    /// caller can pull it after the Ident token.
    pending_colon_arg: Option<String>,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            pos: 0,
            pending_colon_arg: None,
        }
    }

    fn rest(&self) -> &str {
        &self.src[self.pos..]
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.rest().chars().next() {
            if c.is_whitespace() {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
    }

    fn next_token(&mut self) -> Result<Option<Token>, String> {
        self.skip_ws();
        let mut chars = self.rest().chars();
        let Some(c) = chars.next() else {
            return Ok(None);
        };
        if c == '"' || c == '\'' {
            return self.read_quoted(c).map(Some);
        }
        if c == '@' {
            return self.read_at_color().map(Some);
        }
        if c == '[' && self.rest().starts_with("[#") {
            return self.read_bracketed_hex().map(Some);
        }
        if is_ident_start(c) {
            return self.read_ident().map(Some);
        }
        // Skip `key=…` here? No — `key=` starts with an
        // ident byte, handled above. A bare non-ident byte
        // becomes a single-char literal.
        self.pos += c.len_utf8();
        Ok(Some(Token::Punct(c)))
    }

    /// Read an `@TOKEN` color directive. Token is either
    /// an uppercase color/style name (`@RED`, `@INFO`) or
    /// a hex literal (`@#aabbcc`).
    fn read_at_color(&mut self) -> Result<Token, String> {
        self.pos += 1; // consume '@'
        let start = self.pos;
        while let Some(c) = self.rest().chars().next() {
            if c.is_ascii_alphanumeric() || c == '_' || c == '#' {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
        let raw = &self.src[start..self.pos];
        if raw.is_empty() {
            return Err("readouts: stray `@` with no colour token after it".to_string());
        }
        let spec = ColorSpec::parse(raw).ok_or_else(|| {
            format!(
                "readouts: unknown colour / style '@{raw}' (RED|BRIGHT_RED|#rrggbb|ERROR|INFO|…)"
            )
        })?;
        Ok(Token::ColorDirective(spec))
    }

    /// Read a `[#hex]` bracketed hex color directive.
    fn read_bracketed_hex(&mut self) -> Result<Token, String> {
        self.pos += 1; // consume '['
        let start = self.pos;
        while let Some(c) = self.rest().chars().next() {
            if c == ']' {
                break;
            }
            self.pos += c.len_utf8();
        }
        let raw = &self.src[start..self.pos];
        if !self.rest().starts_with(']') {
            return Err(format!(
                "readouts: unterminated `[…]` color directive at byte {}",
                start - 1
            ));
        }
        self.pos += 1; // consume ']'
        let spec = ColorSpec::parse(raw).ok_or_else(|| {
            format!(
                "readouts: invalid bracketed colour '[{raw}]' (expected `[#rrggbb]` or `[#rgb]`)"
            )
        })?;
        Ok(Token::ColorDirective(spec))
    }

    fn read_quoted(&mut self, quote: char) -> Result<Token, String> {
        let mut chars = self.rest().chars();
        chars.next(); // consume opening quote
        let inner_start = self.pos + quote.len_utf8();
        let mut inner_end = inner_start;
        for c in chars {
            if c == quote {
                break;
            }
            inner_end += c.len_utf8();
        }
        if inner_end >= self.src.len() {
            return Err(format!("unterminated quoted literal at byte {}", self.pos));
        }
        let s = self.src[inner_start..inner_end].to_string();
        self.pos = inner_end + quote.len_utf8();
        Ok(Token::Quoted(s))
    }

    fn read_ident(&mut self) -> Result<Token, String> {
        let start = self.pos;
        while let Some(c) = self.rest().chars().next() {
            if is_ident_cont(c) {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
        let name = self.src[start..self.pos].to_string();

        // Attached colon-arg: `name:arg` where `arg` is a
        // non-whitespace run that may contain glob chars
        // (`*`, `?`), digits, etc. but no `=`.
        if self.rest().starts_with(':') && !self.rest().starts_with(":=")
        // future-proof
        {
            // peek: is the next byte after `:` something
            // that could start a value? Quoted, ident, glob
            // char, digit. If yes, attach.
            let after_colon = &self.src[self.pos + 1..];
            if let Some(c) = after_colon.chars().next()
                && (is_ident_start(c)
                    || c == '"'
                    || c == '\''
                    || c == '*'
                    || c == '?'
                    || c.is_ascii_digit())
            {
                self.pos += 1; // consume ':'
                let arg = self.read_unquoted_value()?;
                self.pending_colon_arg = Some(arg);
            }
        }
        Ok(Token::Ident(name))
    }

    fn consume_attached_colon_arg(&mut self) -> Option<String> {
        self.pending_colon_arg.take()
    }

    /// Look ahead: is the next item a `key=value` (or
    /// `key:value` colon-shorthand for structural keys)
    /// that belongs to the current readout call? SRD-63
    /// Push 9f extends peek to honour the colon form for
    /// `lod:compact` / `layout:block` / `color:RED` /
    /// `style:ERROR` — symmetric with the long form. Bare
    /// names (no separator) terminate option parsing and
    /// start a fresh item per the prior contract.
    fn peek_option(&mut self) -> Option<()> {
        self.skip_ws();
        let rest = self.rest();
        let mut chars = rest.chars();
        let c = chars.next()?;
        if !is_ident_start(c) {
            return None;
        }
        // Walk an identifier and check for `=` or `:` after it.
        let mut idx = c.len_utf8();
        for c in chars {
            if is_ident_cont(c) {
                idx += c.len_utf8();
            } else {
                break;
            }
        }
        let after = &rest[idx..];
        let key = &rest[..idx];
        if after.starts_with('=') {
            Some(())
        } else if after.starts_with(':') && is_structural_key(key) {
            // Only structural keys (lod / layout / color /
            // style) accept the colon shorthand. Other
            // `name:value` patterns are reserved for the
            // per-readout primary-arg shorthand (see
            // `consume_attached_colon_arg`).
            Some(())
        } else {
            None
        }
    }

    fn consume_option_pair(&mut self) -> Result<(String, OptionValue), String> {
        self.skip_ws();
        // Read key.
        let start = self.pos;
        while let Some(c) = self.rest().chars().next() {
            if is_ident_cont(c) {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
        let key = self.src[start..self.pos].to_string();
        // Accept either `=` or `:` (the latter only for
        // structural keys, matching `peek_option`'s gate).
        match self.rest().chars().next() {
            Some('=') => {
                self.pos += 1;
            }
            Some(':') if is_structural_key(&key) => {
                self.pos += 1;
            }
            _ => {
                return Err(format!(
                    "expected `=` (or `:` for lod/layout/color/style) after option key '{key}'"
                ));
            }
        }

        // Read value: quoted or unquoted.
        let value = match self.rest().chars().next() {
            Some('"') => OptionValue::Str(self.read_quoted_value('"')?),
            Some('\'') => OptionValue::Str(self.read_quoted_value('\'')?),
            Some(_) => {
                let raw = self.read_unquoted_value()?;
                // Try int → float → bool → string.
                if let Ok(i) = raw.parse::<i64>() {
                    OptionValue::Int(i)
                } else if let Ok(f) = raw.parse::<f64>() {
                    OptionValue::Float(f)
                } else if raw == "true" {
                    OptionValue::Bool(true)
                } else if raw == "false" {
                    OptionValue::Bool(false)
                } else {
                    OptionValue::Str(raw)
                }
            }
            None => return Err(format!("missing value for option '{key}'")),
        };
        Ok((key, value))
    }
}

/// True for the four structural option keys that accept
/// the `key:value` colon-shorthand (alongside the long
/// `key=value` form). Other keys must use `=` so the
/// per-readout primary-arg shorthand
/// (`metric:recall*` ≡ `metric pattern="recall*"`) can
/// stay unambiguous.
fn is_structural_key(key: &str) -> bool {
    matches!(key, "lod" | "layout" | "color" | "style")
}

impl<'a> Lexer<'a> {
    fn read_quoted_value(&mut self, quote: char) -> Result<String, String> {
        // Same logic as read_quoted but returns just the string.
        let inner_start = self.pos + quote.len_utf8();
        let mut chars = self.src[self.pos..].chars();
        chars.next(); // open quote
        let mut inner_end = inner_start;
        for c in chars {
            if c == quote {
                break;
            }
            inner_end += c.len_utf8();
        }
        if inner_end >= self.src.len() {
            return Err(format!("unterminated quoted value at byte {}", self.pos));
        }
        let s = self.src[inner_start..inner_end].to_string();
        self.pos = inner_end + quote.len_utf8();
        Ok(s)
    }

    fn read_unquoted_value(&mut self) -> Result<String, String> {
        let start = self.pos;
        while let Some(c) = self.rest().chars().next() {
            if c.is_whitespace() || c == '=' {
                break;
            }
            self.pos += c.len_utf8();
        }
        Ok(self.src[start..self.pos].to_string())
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_cont(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

// `Chars` import not used here, but kept available for
// future reset / lookahead helpers.
#[allow(dead_code)]
fn _unused_chars_marker(_: Chars<'_>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_readout_name() {
        let (baked, _) = bake("phase_outcome").expect("parse");
        // One step: a single render call.
        assert_eq!(baked.steps.len(), 1);
        assert!(matches!(baked.steps[0], RenderStep::Render { .. }));
    }

    #[test]
    fn parses_multiple_readouts_with_joining_space() {
        // phase_outcome phase_status → render, " ", render
        let (baked, _) = bake("phase_outcome phase_status").expect("parse");
        assert_eq!(baked.steps.len(), 3);
        match &baked.steps[1] {
            RenderStep::Literal(s) => assert_eq!(s, " "),
            _ => panic!("expected joining literal at index 1"),
        }
    }

    #[test]
    fn parses_quoted_literal() {
        let (baked, _) = bake(r#"phase_outcome "ok:" phase_status"#).expect("parse");
        // render, " ", literal "ok:", " ", render
        assert_eq!(baked.steps.len(), 5);
        match &baked.steps[2] {
            RenderStep::Literal(s) => assert_eq!(s, "ok:"),
            other => panic!("expected literal, got {:?}", other.discriminant()),
        }
    }

    #[test]
    fn unknown_readout_name_is_error() {
        let err = bake("not_a_real_readout").unwrap_err();
        assert!(
            err.contains("unknown readout name 'not_a_real_readout'"),
            "wrong message: {err}"
        );
    }

    #[test]
    fn parses_lod_option() {
        let (baked, _) = bake("phase_status lod=compact").expect("parse");
        match &baked.steps[0] {
            RenderStep::Render { lod, .. } => {
                assert_eq!(*lod, Lod::Compact);
            }
            _ => panic!("expected Render"),
        }
    }

    #[test]
    fn parses_lod_numeric_alias() {
        let (baked, _) = bake("phase_status lod=2").expect("parse");
        match &baked.steps[0] {
            RenderStep::Render { lod, .. } => assert_eq!(*lod, Lod::Labeled),
            _ => panic!("expected Render"),
        }
    }

    #[test]
    fn parses_layout_option() {
        let (baked, _) = bake("phase_status layout=block").expect("parse");
        match &baked.steps[0] {
            RenderStep::Render { layout, .. } => {
                assert_eq!(*layout, LayoutMode::Block);
            }
            _ => panic!("expected Render"),
        }
    }

    #[test]
    fn rejects_unknown_lod_value() {
        let err = bake("phase_status lod=enormous").unwrap_err();
        assert!(err.contains("unknown LOD"), "wrong message: {err}");
    }

    #[test]
    fn rejects_unknown_layout_value() {
        let err = bake("phase_status layout=sideways").unwrap_err();
        assert!(err.contains("unknown layout"), "wrong message: {err}");
    }

    #[test]
    fn parses_colon_shorthand_arg() {
        // metric:recall* — colon shorthand attaches the
        // arg to the readout call. Push 5 will consume
        // it inside the metric readout; for now we just
        // verify the lexer doesn't choke and the readout
        // bakes.
        let (baked, _) = bake("metric:recall*").expect("parse");
        assert!(matches!(baked.steps[0], RenderStep::Render { .. }));
    }

    #[test]
    fn parses_lod_colon_shorthand() {
        // Push 9f: structural keys (lod / layout / color /
        // style) accept the `key:value` shorthand
        // alongside `key=value`.
        let (baked, _) = bake("phase_status lod:compact").expect("parse");
        match &baked.steps[0] {
            RenderStep::Render { lod, .. } => assert_eq!(*lod, Lod::Compact),
            _ => panic!("expected Render"),
        }
    }

    #[test]
    fn parses_layout_colon_shorthand() {
        let (baked, _) = bake("phase_status layout:block").expect("parse");
        match &baked.steps[0] {
            RenderStep::Render { layout, .. } => {
                assert_eq!(*layout, LayoutMode::Block);
            }
            _ => panic!("expected Render"),
        }
    }

    #[test]
    fn parses_color_colon_shorthand() {
        let (baked, _) = bake("phase_outcome color:RED").expect("parse");
        match &baked.steps[0] {
            RenderStep::Render { color: Some(c), .. } => {
                assert_eq!(*c, ColorSpec::Direct("RED"));
            }
            _ => panic!("expected Render with colour"),
        }
    }

    #[test]
    fn parses_mixed_colon_and_equals_options() {
        // Lexer accepts both forms in the same call.
        let (baked, _) = bake("phase_status lod:compact layout=block").expect("parse");
        match &baked.steps[0] {
            RenderStep::Render { lod, layout, .. } => {
                assert_eq!(*lod, Lod::Compact);
                assert_eq!(*layout, LayoutMode::Block);
            }
            _ => panic!("expected Render"),
        }
    }

    #[test]
    fn non_structural_colon_is_primary_arg_not_option() {
        // `metric:recall*` is the primary-arg shorthand
        // (folds into the `pattern` option), NOT a generic
        // `key:value` option syntax. Confirm by checking the
        // baked options carry `pattern`, not the literal
        // bare `metric` key.
        let (baked, _) = bake("metric:recall*").expect("parse");
        match &baked.steps[0] {
            RenderStep::Render { options, .. } => {
                assert_eq!(options.get_str("pattern"), Some("recall*"));
            }
            _ => panic!("expected Render"),
        }
    }

    #[test]
    fn parses_at_color_directive() {
        let (baked, _) = bake("@RED phase_outcome").expect("parse");
        // ColorDirective + Render (no leading joining
        // space because directives are zero-width).
        assert_eq!(baked.steps.len(), 2);
        match &baked.steps[0] {
            RenderStep::ColorDirective(c) => {
                assert_eq!(*c, ColorSpec::Direct("RED"));
            }
            other => panic!("expected ColorDirective, got {other:?}"),
        }
        assert!(matches!(baked.steps[1], RenderStep::Render { .. }));
    }

    #[test]
    fn parses_at_hex_color() {
        let (baked, _) = bake("@#7AC166 phase_outcome").expect("parse");
        match &baked.steps[0] {
            RenderStep::ColorDirective(c) => {
                assert_eq!(*c, ColorSpec::Rgb(0x7A, 0xC1, 0x66));
            }
            other => panic!("expected ColorDirective, got {other:?}"),
        }
    }

    #[test]
    fn parses_bracketed_hex_color() {
        let (baked, _) = bake("[#FFF] phase_outcome").expect("parse");
        match &baked.steps[0] {
            RenderStep::ColorDirective(c) => {
                assert_eq!(*c, ColorSpec::Rgb(0xFF, 0xFF, 0xFF));
            }
            other => panic!("expected ColorDirective, got {other:?}"),
        }
    }

    #[test]
    fn parses_at_style_name() {
        let (baked, _) = bake("@ERROR phase_outcome").expect("parse");
        match &baked.steps[0] {
            RenderStep::ColorDirective(c) => {
                assert_eq!(*c, ColorSpec::Style(super::super::color::StyleName::Error));
            }
            other => panic!("expected ColorDirective, got {other:?}"),
        }
    }

    #[test]
    fn parses_color_option() {
        let (baked, _) = bake("phase_outcome color=BLUE").expect("parse");
        match &baked.steps[0] {
            RenderStep::Render { color, .. } => {
                assert_eq!(*color, Some(ColorSpec::Direct("BLUE")));
            }
            _ => panic!("expected Render"),
        }
    }

    #[test]
    fn parses_style_option() {
        let (baked, _) = bake("phase_outcome style=ERROR").expect("parse");
        match &baked.steps[0] {
            RenderStep::Render { color, .. } => {
                assert_eq!(
                    *color,
                    Some(ColorSpec::Style(super::super::color::StyleName::Error))
                );
            }
            _ => panic!("expected Render"),
        }
    }

    #[test]
    fn unknown_color_token_is_error() {
        let err = bake("@notacolor phase_outcome").unwrap_err();
        assert!(
            err.contains("unknown colour / style '@notacolor'"),
            "wrong message: {err}"
        );
    }

    #[test]
    fn unknown_style_option_is_error() {
        let err = bake("phase_outcome style=notastyle").unwrap_err();
        assert!(
            err.contains("unknown colour / style"),
            "wrong message: {err}"
        );
    }

    /// Test-only diagnostic trait: emits a one-token name for
    /// each `RenderStep` variant when an assertion fails.
    /// Lives inside the test module so the compiler doesn't
    /// flag it as dead code on production builds.
    trait DebugDiscriminant {
        fn discriminant(&self) -> &'static str;
    }
    impl DebugDiscriminant for RenderStep {
        fn discriminant(&self) -> &'static str {
            match self {
                RenderStep::Literal(_) => "Literal",
                RenderStep::Render { .. } => "Render",
                RenderStep::ColorDirective(_) => "ColorDirective",
            }
        }
    }
}
