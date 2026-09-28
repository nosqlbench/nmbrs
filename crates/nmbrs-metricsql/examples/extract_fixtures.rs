// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Extract MetricsQL parser-test fixtures from upstream Go test files into
//! the JSON the parity tests (`tests/parity.rs`) consume.
//!
//! ```text
//! cargo run -p nmbrs-metricsql --example extract_fixtures -- [UPSTREAM_DIR]
//! ```
//!
//! `UPSTREAM_DIR` is a checkout of <https://github.com/VictoriaMetrics/metricsql>;
//! it defaults to `links/metricsql` at the workspace root.
//!
//! The upstream test files use two helper closures:
//!
//! - `same(s)`: parse `s`, re-print, expect `s` unchanged
//! - `another(s, want)`: parse `s`, re-print, expect `want`
//!
//! Every call whose arguments are all string literals becomes one case in
//! `tests/fixtures/<name>.json`:
//!
//! ```text
//! { "source": "parser_test.go", "helper": "same_or_another",
//!   "round_trip": [ { "input": "...", "expected": "..." }, ... ] }
//! ```

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const TARGETS: &[(&str, &str)] = &[
    ("parser_test.go", "parser_round_trip.json"),
    ("prettifier_test.go", "prettifier_round_trip.json"),
];

#[derive(Serialize)]
struct Fixture<'a> {
    source: &'a str,
    helper: &'a str,
    round_trip: Vec<Case>,
}

#[derive(Serialize)]
struct Case {
    input: String,
    expected: String,
}

fn main() -> ExitCode {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let upstream = match std::env::args_os().nth(1) {
        Some(dir) => PathBuf::from(dir),
        None => crate_dir.join("../../links/metricsql"),
    };
    if !upstream.is_dir() {
        eprintln!(
            "ERROR: upstream metricsql not found at {}",
            upstream.display()
        );
        return ExitCode::FAILURE;
    }
    let out_dir = crate_dir.join("tests/fixtures");
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("ERROR: create {}: {e}", out_dir.display());
        return ExitCode::FAILURE;
    }

    let mut total = 0;
    for (src_name, out_name) in TARGETS {
        let src_path = upstream.join(src_name);
        let Ok(src) = std::fs::read_to_string(&src_path) else {
            println!("skip {src_name}: not present");
            continue;
        };
        let cases = harvest_round_trip(&src);
        let n = cases.len();
        let fixture = Fixture {
            source: src_name,
            helper: "same_or_another",
            round_trip: cases,
        };
        let mut json = serde_json::to_string_pretty(&fixture).expect("fixture serializes");
        json.push('\n');
        let out = out_dir.join(out_name);
        if let Err(e) = std::fs::write(&out, json) {
            eprintln!("ERROR: write {}: {e}", out.display());
            return ExitCode::FAILURE;
        }
        println!("{out_name}: {n} cases");
        total += n;
    }
    println!("total: {total} round-trip cases");
    ExitCode::SUCCESS
}

/// Pull `same(s)` and `another(s, expected)` from a Go test file.
fn harvest_round_trip(src: &str) -> Vec<Case> {
    let text: Vec<char> = src.chars().collect();
    let mut cases: Vec<Case> = harvest_calls(&text, "same", 1)
        .into_iter()
        .map(|mut a| {
            let s = a.remove(0);
            Case {
                input: s.clone(),
                expected: s,
            }
        })
        .collect();
    cases.extend(harvest_calls(&text, "another", 2).into_iter().map(|mut a| {
        let expected = a.remove(1);
        Case {
            input: a.remove(0),
            expected,
        }
    }));
    cases
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

/// Every `name(...)` call in `text` whose `arg_count` arguments are all Go
/// string literals, in source order. A trailing comma after the last
/// argument is accepted.
fn harvest_calls(text: &[char], name: &str, arg_count: usize) -> Vec<Vec<String>> {
    let name: Vec<char> = name.chars().collect();
    let skip_ws = |mut i: usize| {
        while i < text.len() && is_space(text[i]) {
            i += 1;
        }
        i
    };
    let mut calls = Vec::new();
    let mut at = 0;
    while at + name.len() <= text.len() {
        let start = at;
        at += 1;
        // `name` as a whole word, then optional whitespace, then `(`.
        if text[start..start + name.len()] != name[..] || (start > 0 && is_word(text[start - 1])) {
            continue;
        }
        let mut i = start + name.len();
        while i < text.len() && text[i].is_whitespace() {
            i += 1;
        }
        if text.get(i) != Some(&'(') {
            continue;
        }
        i += 1;

        let mut args = Vec::with_capacity(arg_count);
        let mut ok = true;
        for arg_idx in 0..arg_count {
            i = skip_ws(i);
            let Some((value, end)) = parse_go_string(text, i) else {
                ok = false;
                break;
            };
            args.push(value);
            i = skip_ws(end);
            if arg_idx + 1 < arg_count {
                if text.get(i) == Some(&',') {
                    i += 1;
                } else {
                    ok = false;
                    break;
                }
            } else {
                if text.get(i) == Some(&',') {
                    i = skip_ws(i + 1);
                }
                if text.get(i) != Some(&')') {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            calls.push(args);
        }
    }
    calls
}

/// Parse a Go string literal starting at `text[pos]`: an interpreted
/// `"..."` string with escape processing, or a raw `` `...` `` string with
/// none. Returns the value and the index just past the closing delimiter,
/// or `None` if `text[pos]` doesn't start a complete literal.
///
/// Carriage returns in raw strings are discarded, as Go does. Octal escapes
/// (`\NNN`) and unknown escapes keep their backslash.
fn parse_go_string(text: &[char], pos: usize) -> Option<(String, usize)> {
    match *text.get(pos)? {
        '`' => {
            let len = text[pos + 1..].iter().position(|&c| c == '`')?;
            let end = pos + 1 + len;
            // Go discards carriage returns inside raw strings, so a CRLF
            // checkout of upstream harvests the same values as an LF one.
            let value = text[pos + 1..end].iter().filter(|&&c| c != '\r').collect();
            Some((value, end + 1))
        }
        '"' => {
            let mut out = String::new();
            let mut i = pos + 1;
            while i < text.len() {
                let c = text[i];
                if c == '"' {
                    return Some((out, i + 1));
                }
                if c != '\\' {
                    out.push(c);
                    i += 1;
                    continue;
                }
                let next = *text.get(i + 1)?;
                let simple = match next {
                    'n' => Some('\n'),
                    't' => Some('\t'),
                    'r' => Some('\r'),
                    '0' => Some('\0'),
                    'a' => Some('\x07'),
                    'b' => Some('\x08'),
                    'f' => Some('\x0c'),
                    'v' => Some('\x0b'),
                    '\\' | '"' | '\'' | '`' => Some(next),
                    _ => None,
                };
                if let Some(ch) = simple {
                    out.push(ch);
                    i += 2;
                    continue;
                }
                // \xHH (one byte), \uHHHH, \UHHHHHHHH: the hex digits name a
                // code point.
                let hex_len = match next {
                    'x' => 2,
                    'u' => 4,
                    'U' => 8,
                    _ => 0,
                };
                if hex_len > 0 && i + 1 + hex_len < text.len() {
                    let hex: String = text[i + 2..i + 2 + hex_len].iter().collect();
                    let code = u32::from_str_radix(&hex, 16).ok()?;
                    out.push(char::from_u32(code)?);
                    i += 2 + hex_len;
                    continue;
                }
                out.push(c);
                i += 1;
            }
            None
        }
        _ => None,
    }
}
