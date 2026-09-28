// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! MetricsQL lexer. Token stream over a query string. Mirrors
//! upstream `metricsql.lexer` — the parser walks tokens via
//! index advancement on the returned `Vec<Token>`.
//!
//! ## Design
//!
//! Each token carries its **verbatim source slice** (`raw`)
//! and the byte offset where it starts in the original input.
//! The lexer doesn't classify tokens by kind — the parser
//! inspects `raw` to discriminate (an identifier vs the
//! keyword `offset`, an integer vs a duration, …). This
//! matches upstream's design exactly and keeps round-trip
//! parity tight.
//!
//! ## Recognition order (mirroring upstream `next()`)
//!
//! 1. Skip whitespace + `# ...\n` line comments
//! 2. Single-char punctuators: `{ } [ ] ( ) , @`
//! 3. Identifier (with `\` escape support, Unicode)
//! 4. String literal (`"..."`, `'...'`, `` `...` ``)
//! 5. Binary-op prefix (longest match: `==`, `!=`, `<=`, `>=`,
//!    `+`, `-`, `*`, `/`, `%`, `^`, plus keyword ops
//!    `and`/`or`/`unless`/`if`/`ifnot`/`default`/`atan2`)
//! 6. Tag-filter op prefix: `=~`, `!~`, `!=`, `=`
//! 7. Duration (must precede number — `5m` is a duration,
//!    not number+identifier)
//! 8. Number (decimal/hex/octal/binary, scientific notation,
//!    byte multipliers KB/MiB/etc.)
//! 9. `$__interval` / `$__rate_interval` magic identifiers
//!
//! Anything else is a lex error.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Verbatim slice from the input. Empty `raw` means EOF.
    pub raw: String,
    /// Byte offset where this token starts in the original input.
    pub start: usize,
}

impl Token {
    pub fn is_eof(&self) -> bool {
        self.raw.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LexError {
    pub message: String,
    pub at: usize,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lex error at byte {}: {}", self.at, self.message)
    }
}

impl std::error::Error for LexError {}

/// Tokenize a MetricsQL query into a flat token vector
/// terminated by an EOF sentinel (a token with empty `raw`).
pub fn lex(input: &str) -> Result<Vec<Token>, LexError> {
    let mut tokens = Vec::new();
    let mut i = 0usize;
    let bytes = input.as_bytes();
    loop {
        i = skip_ws_and_comments(bytes, i);
        if i >= bytes.len() {
            tokens.push(Token {
                raw: String::new(),
                start: i,
            });
            return Ok(tokens);
        }
        let start = i;
        // Single-char punctuators that don't overlap with op
        // prefixes.
        if matches!(
            bytes[i],
            b'{' | b'}' | b'[' | b']' | b'(' | b')' | b',' | b'@'
        ) {
            tokens.push(Token {
                raw: (bytes[i] as char).to_string(),
                start,
            });
            i += 1;
            continue;
        }
        if let Some(end) = scan_ident(input, i) {
            tokens.push(Token {
                raw: input[i..end].to_string(),
                start,
            });
            i = end;
            continue;
        }
        if let Some(end) = scan_string(input, i)? {
            tokens.push(Token {
                raw: input[i..end].to_string(),
                start,
            });
            i = end;
            continue;
        }
        // Binary-op prefix vs tag-filter op vs duration vs
        // number — order matters. Upstream tries binary-op
        // first, then tag-filter ops, then duration, then
        // positive number.
        if let Some(end) = scan_binary_op_prefix(input, i) {
            tokens.push(Token {
                raw: input[i..end].to_string(),
                start,
            });
            i = end;
            continue;
        }
        if let Some(end) = scan_tag_filter_op_prefix(input, i) {
            tokens.push(Token {
                raw: input[i..end].to_string(),
                start,
            });
            i = end;
            continue;
        }
        if let Some(end) = scan_duration(input, i) {
            tokens.push(Token {
                raw: input[i..end].to_string(),
                start,
            });
            i = end;
            continue;
        }
        if is_positive_number_prefix(bytes, i) {
            let end = scan_positive_number(input, i)?;
            tokens.push(Token {
                raw: input[i..end].to_string(),
                start,
            });
            i = end;
            continue;
        }
        if input[i..].starts_with("$__interval") {
            i += "$__interval".len();
            tokens.push(Token {
                raw: "$__interval".into(),
                start,
            });
            continue;
        }
        if input[i..].starts_with("$__rate_interval") {
            // Upstream collapses both into "$__interval"; we
            // do the same so the parser doesn't need a second
            // arm.
            i += "$__rate_interval".len();
            tokens.push(Token {
                raw: "$__interval".into(),
                start,
            });
            continue;
        }
        return Err(LexError {
            message: format!("cannot recognize {:?}", &input[i..]),
            at: i,
        });
    }
}

// ---------------------------------------------------------------------------
// Whitespace / comments
// ---------------------------------------------------------------------------

fn skip_ws_and_comments(bytes: &[u8], mut i: usize) -> usize {
    loop {
        while i < bytes.len() && is_space_byte(bytes[i]) {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'#' {
            // Comment runs to end-of-line. If no newline, EOF.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            if j >= bytes.len() {
                return j;
            }
            i = j + 1;
            continue;
        }
        return i;
    }
}

fn is_space_byte(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

// ---------------------------------------------------------------------------
// Identifier
// ---------------------------------------------------------------------------

fn scan_ident(s: &str, start: usize) -> Option<usize> {
    if !is_ident_prefix(s, start) {
        return None;
    }
    let bytes = s.as_bytes();
    let mut i = start;
    let mut first = true;
    while i < bytes.len() {
        let (r, size) = decode_utf8(s, i);
        if size == 0 {
            break;
        }
        if (first && is_first_ident_char(r)) || (!first && is_ident_char(r)) {
            i += size;
            first = false;
            continue;
        }
        if r == '\\' {
            // Escape sequence — consume the `\` and the
            // escaped char.
            i += size;
            let (esc_r, esc_n) = decode_escape_sequence(s, i);
            if esc_r.is_none() {
                // Invalid escape — back up and stop.
                i -= size;
                break;
            }
            i += esc_n;
            first = false;
            continue;
        }
        break;
    }
    if i == start { None } else { Some(i) }
}

fn is_ident_prefix(s: &str, i: usize) -> bool {
    let (r, size) = decode_utf8(s, i);
    if size == 0 {
        return false;
    }
    if r == '\\' {
        let (esc, _) = decode_escape_sequence(s, i + size);
        return esc.is_some();
    }
    is_first_ident_char(r)
}

fn is_first_ident_char(r: char) -> bool {
    if r.is_alphabetic() {
        return true;
    }
    matches!(r, '_' | ':')
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

fn decode_utf8(s: &str, i: usize) -> (char, usize) {
    if i >= s.len() {
        return ('\0', 0);
    }
    let rest = &s[i..];
    if let Some(c) = rest.chars().next() {
        (c, c.len_utf8())
    } else {
        ('\0', 0)
    }
}

/// Decode an escape sequence starting at `s[i]` (the char
/// after the leading `\`). Returns `(Some(rune), bytes_consumed)`
/// on success, `(None, 0)` on a malformed escape.
fn decode_escape_sequence(s: &str, i: usize) -> (Option<char>, usize) {
    if i >= s.len() {
        return (None, 0);
    }
    let bytes = s.as_bytes();
    let head = bytes[i];
    if head == b'x' || head == b'X' {
        if i + 3 > s.len() {
            return (None, 0);
        }
        let h1 = from_hex(bytes[i + 1]);
        let h2 = from_hex(bytes[i + 2]);
        if let (Some(h1), Some(h2)) = (h1, h2) {
            let v = (h1 << 4) | h2;
            return (char::from_u32(v), 3);
        }
        return (None, 0);
    }
    if head == b'u' || head == b'U' {
        if i + 5 > s.len() {
            return (None, 0);
        }
        let h1 = from_hex(bytes[i + 1]);
        let h2 = from_hex(bytes[i + 2]);
        let h3 = from_hex(bytes[i + 3]);
        let h4 = from_hex(bytes[i + 4]);
        if let (Some(h1), Some(h2), Some(h3), Some(h4)) = (h1, h2, h3, h4) {
            let v = (h1 << 12) | (h2 << 8) | (h3 << 4) | h4;
            return (char::from_u32(v), 5);
        }
        return (None, 0);
    }
    let (r, size) = decode_utf8(s, i);
    if size == 0 {
        return (None, 0);
    }
    // Upstream: only printable runes are accepted as escaped
    // chars; non-printable raw bytes are treated as bad.
    // We approximate "printable" via Rust's Unicode category
    // — `is_control` catches the cases upstream rejects.
    if r.is_control() {
        return (None, 0);
    }
    (Some(r), size)
}

fn from_hex(b: u8) -> Option<u32> {
    match b {
        b'0'..=b'9' => Some((b - b'0') as u32),
        b'a'..=b'f' => Some((b - b'a' + 10) as u32),
        b'A'..=b'F' => Some((b - b'A' + 10) as u32),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// String literal
// ---------------------------------------------------------------------------

fn is_string_prefix(b: u8) -> bool {
    matches!(b, b'"' | b'\'' | b'`')
}

fn scan_string(s: &str, start: usize) -> Result<Option<usize>, LexError> {
    let bytes = s.as_bytes();
    if start >= bytes.len() || !is_string_prefix(bytes[start]) {
        return Ok(None);
    }
    let quote = bytes[start];
    let mut i = start + 1;
    loop {
        match bytes[i..].iter().position(|&b| b == quote) {
            None => {
                return Err(LexError {
                    message: format!("cannot find closing quote {} for the string", quote as char),
                    at: start,
                });
            }
            Some(rel) => {
                i += rel;
                // Count preceding backslashes — even number
                // means the quote is unescaped.
                let mut bs = 0;
                while bs < (i - start - 1) && bytes[i - bs - 1] == b'\\' {
                    bs += 1;
                }
                if bs % 2 == 0 {
                    return Ok(Some(i + 1));
                }
                i += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Binary-op + tag-filter-op prefixes
// ---------------------------------------------------------------------------

const SYMBOLIC_BINARY_OPS: &[&str] = &[
    // longest first so the matcher is greedy
    "==", "!=", ">=", "<=", "+", "-", "*", "/", "%", "^", ">", "<",
];

// Keyword binary ops (`and`, `or`, `unless`, `if`, `ifnot`,
// `default`, `atan2`) are lexed as identifiers; the parser
// discriminates by inspecting the raw token text. This
// matches upstream — keeping the lexer's job purely
// structural.

fn scan_binary_op_prefix(s: &str, i: usize) -> Option<usize> {
    let rest = &s[i..];
    // Symbolic ops first — pure prefix match.
    for op in SYMBOLIC_BINARY_OPS {
        if rest.starts_with(op) {
            return Some(i + op.len());
        }
    }
    // Keyword ops — case-insensitive, must be word-boundary
    // ended (so `andy` isn't lexed as `and` + `y`). The lexer
    // delegates that to scan_ident which would have already
    // matched first — but keyword ops are NOT consumed by
    // scan_ident in upstream because scan_ident recognises
    // them too. Upstream's `next()` actually tries
    // scan_ident BEFORE scan_binary_op_prefix; keyword ops
    // come back through the parser inspecting the raw token.
    //
    // We replicate the same flow: scan_ident takes the
    // keyword as an identifier; the parser checks whether
    // the identifier is a binary-op keyword. So this
    // function only needs to handle the symbolic ops.
    None
}

const TAG_FILTER_OPS: &[&str] = &["=~", "!~", "!=", "="];

fn scan_tag_filter_op_prefix(s: &str, i: usize) -> Option<usize> {
    let rest = &s[i..];
    for op in TAG_FILTER_OPS {
        if rest.starts_with(op) {
            return Some(i + op.len());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Duration
// ---------------------------------------------------------------------------

/// External entry point for callers that need to test whether
/// a string is a complete duration literal (e.g. the parser
/// distinguishing standalone duration tokens from numbers).
pub(crate) fn scan_duration_external(s: &str) -> Option<usize> {
    scan_duration(s, 0)
}

fn scan_duration(s: &str, start: usize) -> Option<usize> {
    let n = scan_single_duration(s, start, false)?;
    let mut end = n;
    loop {
        match scan_single_duration(s, end, true) {
            Some(m) => end = m,
            None => return Some(end),
        }
    }
}

/// Upstream's `scanSingleDuration`. Returns the byte offset
/// AFTER the parsed duration unit, or `None` if no duration
/// starts at `s[start]`.
fn scan_single_duration(s: &str, start: usize, can_be_negative: bool) -> Option<usize> {
    let bytes = s.as_bytes();
    if start >= bytes.len() {
        return None;
    }
    let mut i = start;
    if bytes[i] == b'-' && can_be_negative {
        i += 1;
    }
    if s[i..].starts_with("$__interval") {
        return Some(i + "$__interval".len());
    }
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start || i == bytes.len() {
        return None;
    }
    if bytes[i] == b'.' {
        let j = i;
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == j + 1 || i == bytes.len() {
            return None;
        }
    }
    let unit = bytes[i].to_ascii_lowercase();
    match unit {
        b'm' => {
            if i + 1 < bytes.len() {
                let next = bytes[i + 1].to_ascii_lowercase();
                if next == b's' {
                    return Some(i + 2);
                }
                // `Mi` / `Mb` are byte multipliers, not
                // durations.
                if next == b'i' || next == b'b' {
                    return None;
                }
            }
            // `m` (minutes). Upstream restricts to lowercase
            // because `M` means 1e6 multiplier.
            if bytes[i] == b'm' { Some(i + 1) } else { None }
        }
        b's' | b'h' | b'd' | b'w' | b'y' | b'i' => Some(i + 1),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Number
// ---------------------------------------------------------------------------

fn is_positive_number_prefix(bytes: &[u8], i: usize) -> bool {
    if i >= bytes.len() {
        return false;
    }
    if bytes[i].is_ascii_digit() {
        return true;
    }
    // `.234` numbers
    if bytes[i] != b'.' || i + 1 >= bytes.len() {
        return false;
    }
    bytes[i + 1].is_ascii_digit()
}

fn scan_positive_number(s: &str, start: usize) -> Result<usize, LexError> {
    let bytes = s.as_bytes();
    let mut i = start;
    let (skip, is_hex) = scan_special_integer_prefix(bytes, i);
    i += skip;
    if is_hex {
        while i < bytes.len() && is_hex_char(bytes[i]) {
            i += 1;
        }
        return Ok(i);
    }
    while i < bytes.len() && is_decimal_or_underscore(bytes[i]) {
        i += 1;
    }
    if i >= bytes.len() {
        if i == start {
            return Err(LexError {
                message: "number cannot be empty".into(),
                at: start,
            });
        }
        return Ok(i);
    }
    if let Some(m) = scan_num_multiplier(s, i) {
        return Ok(i + m);
    }
    if bytes[i] != b'.' && bytes[i] != b'e' && bytes[i] != b'E' {
        if i == start {
            return Err(LexError {
                message: "missing positive number".into(),
                at: start,
            });
        }
        return Ok(i);
    }
    if bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && is_decimal_or_underscore(bytes[i]) {
            i += 1;
        }
        if i == bytes.len() {
            return Ok(i);
        }
    }
    if let Some(m) = scan_num_multiplier(s, i) {
        return Ok(i + m);
    }
    if bytes[i] != b'e' && bytes[i] != b'E' {
        return Ok(i);
    }
    i += 1;
    if i == bytes.len() {
        return Err(LexError {
            message: "missing exponent part".into(),
            at: start,
        });
    }
    if bytes[i] == b'-' || bytes[i] == b'+' {
        i += 1;
    }
    let exp_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == exp_start {
        return Err(LexError {
            message: "missing exponent part".into(),
            at: start,
        });
    }
    Ok(i)
}

fn scan_num_multiplier(s: &str, i: usize) -> Option<usize> {
    let take = (s.len() - i).min(3);
    let head = s[i..i + take].to_lowercase();
    let head = head.as_str();
    for (suffix, len) in &[
        ("kib", 3),
        ("ki", 2),
        ("kb", 2),
        ("k", 1),
        ("mib", 3),
        ("mi", 2),
        ("mb", 2),
        ("m", 1),
        ("gib", 3),
        ("gi", 2),
        ("gb", 2),
        ("g", 1),
        ("tib", 3),
        ("ti", 2),
        ("tb", 2),
        ("t", 1),
    ] {
        if head.starts_with(suffix) {
            return Some(*len);
        }
    }
    None
}

fn scan_special_integer_prefix(bytes: &[u8], i: usize) -> (usize, bool) {
    if i >= bytes.len() || bytes[i] != b'0' {
        return (0, false);
    }
    if i + 1 >= bytes.len() {
        return (0, false);
    }
    let next = bytes[i + 1].to_ascii_lowercase();
    if next.is_ascii_digit() {
        // Leading-zero octal: 0123 → consume the `0`.
        return (1, false);
    }
    if next == b'x' {
        return (2, true);
    }
    if next == b'o' || next == b'b' {
        return (2, false);
    }
    (0, false)
}

fn is_hex_char(b: u8) -> bool {
    b.is_ascii_digit() || matches!(b, b'a'..=b'f' | b'A'..=b'F')
}

fn is_decimal_or_underscore(b: u8) -> bool {
    b.is_ascii_digit() || b == b'_'
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_tokens(input: &str) -> Vec<String> {
        let toks = lex(input).expect("lex");
        toks.into_iter()
            .filter(|t| !t.is_eof())
            .map(|t| t.raw)
            .collect()
    }

    #[test]
    fn empty_input_is_just_eof() {
        let toks = lex("").unwrap();
        assert_eq!(toks.len(), 1);
        assert!(toks[0].is_eof());
    }

    #[test]
    fn single_char_punctuators() {
        assert_eq!(
            raw_tokens("{}[](),@"),
            vec!["{", "}", "[", "]", "(", ")", ",", "@"]
        );
    }

    #[test]
    fn whitespace_skipped() {
        assert_eq!(raw_tokens("  metric   "), vec!["metric"]);
    }

    #[test]
    fn line_comments_skipped() {
        assert_eq!(raw_tokens("# leading comment\nmetric"), vec!["metric"]);
        assert_eq!(
            raw_tokens("metric # trailing\n + 5"),
            vec!["metric", "+", "5"]
        );
    }

    #[test]
    fn identifier_basic() {
        assert_eq!(raw_tokens("metric"), vec!["metric"]);
        assert_eq!(raw_tokens("foo_bar"), vec!["foo_bar"]);
        assert_eq!(raw_tokens("m_e:tri44:_c123"), vec!["m_e:tri44:_c123"]);
    }

    #[test]
    fn string_double_quoted() {
        assert_eq!(raw_tokens(r#""hello""#), vec![r#""hello""#]);
        assert_eq!(raw_tokens(r#""esc\"aped""#), vec![r#""esc\"aped""#]);
    }

    #[test]
    fn string_single_quoted() {
        assert_eq!(raw_tokens(r#"'foo'"#), vec![r#"'foo'"#]);
    }

    #[test]
    fn number_decimal() {
        assert_eq!(raw_tokens("123"), vec!["123"]);
        assert_eq!(raw_tokens("1.5"), vec!["1.5"]);
        assert_eq!(raw_tokens("1e5"), vec!["1e5"]);
        assert_eq!(raw_tokens("1.3E-3"), vec!["1.3E-3"]);
    }

    #[test]
    fn number_special_bases() {
        assert_eq!(raw_tokens("0x1F"), vec!["0x1F"]);
        assert_eq!(raw_tokens("0b101"), vec!["0b101"]);
        assert_eq!(raw_tokens("0o755"), vec!["0o755"]);
    }

    #[test]
    fn number_byte_multipliers() {
        assert_eq!(raw_tokens("5KB"), vec!["5KB"]);
        assert_eq!(raw_tokens("3MiB"), vec!["3MiB"]);
        assert_eq!(raw_tokens("1.5G"), vec!["1.5G"]);
    }

    #[test]
    fn duration_simple() {
        assert_eq!(raw_tokens("5m"), vec!["5m"]);
        assert_eq!(raw_tokens("1h30m"), vec!["1h30m"]);
        assert_eq!(raw_tokens("100ms"), vec!["100ms"]);
        assert_eq!(raw_tokens("3.5d"), vec!["3.5d"]);
    }

    #[test]
    fn duration_combined_with_negative() {
        // `2h-5m` is a single combined duration.
        assert_eq!(raw_tokens("2h-5m"), vec!["2h-5m"]);
    }

    #[test]
    fn binary_ops_symbolic() {
        assert_eq!(raw_tokens("1 + 2"), vec!["1", "+", "2"]);
        assert_eq!(raw_tokens("a == b"), vec!["a", "==", "b"]);
        assert_eq!(raw_tokens("x >= y"), vec!["x", ">=", "y"]);
    }

    #[test]
    fn tag_filter_ops() {
        assert_eq!(raw_tokens(r#"{a="b"}"#), vec!["{", "a", "=", r#""b""#, "}"]);
        assert_eq!(
            raw_tokens(r#"{a!="b"}"#),
            vec!["{", "a", "!=", r#""b""#, "}"]
        );
        assert_eq!(
            raw_tokens(r#"{a=~"b"}"#),
            vec!["{", "a", "=~", r#""b""#, "}"]
        );
        assert_eq!(
            raw_tokens(r#"{a!~"b"}"#),
            vec!["{", "a", "!~", r#""b""#, "}"]
        );
    }

    #[test]
    fn selector_with_offset() {
        assert_eq!(
            raw_tokens(r#"metric{foo="bar"} offset 5m"#),
            vec!["metric", "{", "foo", "=", r#""bar""#, "}", "offset", "5m"],
        );
    }

    #[test]
    fn dollar_interval_and_rate_interval_collapse() {
        assert_eq!(raw_tokens("$__interval"), vec!["$__interval"]);
        // `$__rate_interval` is collapsed to `$__interval`
        // matching upstream.
        assert_eq!(raw_tokens("$__rate_interval"), vec!["$__interval"]);
    }

    #[test]
    fn unrecognized_input_errors() {
        let err = lex("§").unwrap_err();
        assert_eq!(err.at, 0);
    }

    #[test]
    fn unicode_identifier() {
        // Upstream allows non-ASCII identifiers via UTF-8.
        assert_eq!(raw_tokens("温度"), vec!["温度"]);
    }
}
