// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0
//
// Round-trip parity tests against upstream's parser_test.go
// and prettifier_test.go. The fixture JSON is harvested by
// `scripts/extract_fixtures.py`; each case is a
// (input, expected) pair where `parse(input)` followed by
// `pretty_string(...)` must equal `expected`.
//
// Until the parser is implemented these tests are wired but
// gated on `RUN_METRICSQL_PARITY=1` so a clean
// `cargo test --workspace` doesn't drown in 551 known failures
// during the porting effort. CI / the parity harness flips
// the env var on once a milestone lands.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct Fixture {
    source: String,
    round_trip: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    input: String,
    expected: String,
}

fn load_fixture(name: &str) -> Fixture {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// Which parse path to feed each fixture's `input` through.
/// Parser fixtures use the full `parse` (WITH expansion +
/// constant folding); prettifier fixtures use
/// `parse_for_prettify` so the round-trip can preserve
/// `WITH (...)` templates and unfolded literal subtrees.
#[derive(Copy, Clone)]
enum ParseMode {
    Full,
    Prettify,
}

fn parse_with_mode(input: &str, mode: ParseMode) -> Result<nmbrs_metricsql::ast::Expr, String> {
    match mode {
        ParseMode::Full => nmbrs_metricsql::parse(input).map_err(|e| e.to_string()),
        ParseMode::Prettify => {
            nmbrs_metricsql::parse_for_prettify(input).map_err(|e| e.to_string())
        }
    }
}

fn run_round_trip(fixture: &str) {
    let mode = if fixture.starts_with("prettifier") {
        ParseMode::Prettify
    } else {
        ParseMode::Full
    };
    run_round_trip_with(fixture, mode)
}

fn run_round_trip_with(fixture: &str, parse_mode: ParseMode) {
    // Modes:
    //   (default)             — load only, log how many cases the
    //                           harness would run.
    //   RUN_METRICSQL_PARITY=count
    //                         — run all cases, report counts, never fail.
    //                           For tracking progress as the parser ports.
    //   RUN_METRICSQL_PARITY=strict
    //                         — run all cases, fail on the first mismatch.
    let mode = std::env::var("RUN_METRICSQL_PARITY").unwrap_or_default();
    if mode.is_empty() {
        let fx = load_fixture(fixture);
        eprintln!(
            "{}: {} round-trip cases (set RUN_METRICSQL_PARITY=count to exercise)",
            fx.source,
            fx.round_trip.len()
        );
        return;
    }

    let fx = load_fixture(fixture);
    let mut passed = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for case in &fx.round_trip {
        match parse_with_mode(&case.input, parse_mode) {
            Ok(ast) => {
                let got = match parse_mode {
                    ParseMode::Full => nmbrs_metricsql::prettifier::pretty_string(&ast),
                    ParseMode::Prettify => nmbrs_metricsql::prettifier::pretty_print(&ast),
                };
                if got == case.expected {
                    passed += 1;
                } else {
                    failures.push(format!(
                        "  in : {:?}\n    want: {:?}\n    got : {:?}",
                        case.input, case.expected, got
                    ));
                }
            }
            Err(e) => failures.push(format!("  in : {:?}\n    parse error: {e}", case.input)),
        }
    }
    let total = fx.round_trip.len();
    let pct = if total == 0 {
        0.0
    } else {
        passed as f64 * 100.0 / total as f64
    };
    eprintln!("{}: {}/{} ({:.1}%) passed", fx.source, passed, total, pct);
    if mode == "strict" && !failures.is_empty() {
        let limit: usize = std::env::var("RUN_METRICSQL_PARITY_LIMIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20);
        for f in failures.iter().take(limit) {
            eprintln!("{f}");
        }
        if failures.len() > limit {
            eprintln!("... and {} more failures", failures.len() - limit);
        }
        panic!(
            "{} round-trip cases failed in {} (strict mode)",
            failures.len(),
            fx.source
        );
    }
}

#[test]
fn parser_round_trip() {
    run_round_trip("parser_round_trip.json");
}

#[test]
fn prettifier_round_trip() {
    run_round_trip("prettifier_round_trip.json");
}
