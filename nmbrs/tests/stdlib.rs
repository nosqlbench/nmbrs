// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-12 stdlib node-catalog coverage tests.
//!
//! Each test runs one scenario from
//! `nmbrs/examples/workloads/expressions/stdlib_coverage.yaml` and asserts the
//! exact emitted line for one family of stdlib functions:
//!
//! - Arithmetic (u64 infix / f64 infix / named-node form)
//! - Bitwise infix (&, |, ^, <<, >>)
//! - Comparison ops (==, !=, <, >, <=, >=)
//! - Type conversion (to_f64, to_u64, format_u64)
//! - Hashing (hash determinism)
//! - String interpolation
//! - Encoding round-trips (hex, base64, url)
//! - Digests (sha256, md5)
//! - Probability + distributions
//! - Weighted selection
//! - Pick / select / blend
//! - Lerp / quantize / clamp
//! - Noise (perlin_2d)
//! - Date/time
//! - JSON
//! - Regex

use std::path::{Path, PathBuf};
use std::process::Command;

const WORKLOAD: &str = "nmbrs/examples/workloads/expressions/stdlib_coverage.yaml";

struct SessionDir {
    path: PathBuf,
}

impl SessionDir {
    fn new() -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent = std::env::temp_dir().join(format!("nmbrs-stdlib-coverage-{pid}-{nanos}"));
        std::fs::create_dir_all(&parent).expect("create session parent");
        Self {
            path: parent.join("session"),
        }
    }
    fn parent(&self) -> &Path {
        self.path.parent().unwrap()
    }
}

impl Drop for SessionDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.parent());
    }
}

fn run_scenario(scenario: &str) -> (String, String, bool) {
    let session = SessionDir::new();
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    cmd.current_dir(workspace_root)
        .arg("run")
        .arg("--session-path")
        .arg(&session.path)
        .arg(format!("workload={WORKLOAD}"))
        .arg(format!("scenario={scenario}"));
    let out = cmd.output().expect("run nmbrs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    (stdout, stderr, out.status.success())
}

/// First line matching the given prefix, or panic with full
/// stdout context.
fn first_line(stdout: &str, prefix: &str) -> String {
    stdout
        .lines()
        .find(|l| l.starts_with(prefix))
        .map(|l| l.to_string())
        .unwrap_or_else(|| panic!("no line with prefix `{prefix}` in:\n{stdout}"))
}

// ─────────────────────────────────────────────────────────────────
// Arithmetic
// ─────────────────────────────────────────────────────────────────

#[test]
fn stdlib_arithmetic_u64() {
    // 10 + 3 = 13, 10 - 3 = 7, 10 * 3 = 30, 10 / 3 = 3 (integer),
    // 10 % 3 = 1, 10 ** 2 = 100. `pow` returns f64 even for
    // integer operands, so it renders with the explicit-float
    // suffix (whole-number floats are `<n>.0` in display per
    // `Value::F64::to_display_string`).
    let (stdout, stderr, ok) = run_scenario("arithmetic_u64");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        first_line(&stdout, "lib/arith_u64 "),
        "lib/arith_u64 sum=13 diff=7 prod=30 quot=3 rem=1 pow=100.0"
    );
}

#[test]
fn stdlib_arithmetic_f64() {
    // 2 + 8 = 10, 8 - 2 = 6, 2 * 8 = 16, 8 / 2 = 4, 2 ** 3 = 8.
    // Each result is f64; whole-number floats render with the
    // explicit `.0` suffix per `Value::F64::to_display_string`.
    let (stdout, stderr, ok) = run_scenario("arithmetic_f64");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        first_line(&stdout, "lib/arith_f64 "),
        "lib/arith_f64 sum=10.0 diff=6.0 prod=16.0 quot=4.0 power=8.0"
    );
}

#[test]
fn stdlib_arithmetic_named_nodes() {
    // Same values as infix; verifies named-node form parses and
    // computes identically.
    let (stdout, stderr, ok) = run_scenario("arithmetic_named_nodes");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        first_line(&stdout, "lib/arith_named "),
        "lib/arith_named sum=13 prod=30 quot=3 rem=1"
    );
}

// ─────────────────────────────────────────────────────────────────
// Bitwise
// ─────────────────────────────────────────────────────────────────

#[test]
fn stdlib_bitwise_ops() {
    // 0xF0 = 240, 0x0F = 15.
    //   AND = 0, OR = 255, XOR = 255.
    //   shl 4 = 0xF00 = 3840, shr 4 = 0x0F = 15.
    let (stdout, stderr, ok) = run_scenario("bitwise_ops");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        first_line(&stdout, "lib/bitwise "),
        "lib/bitwise and=0 or=255 xor=255 shl=3840 shr=15"
    );
}

// ─────────────────────────────────────────────────────────────────
// Comparison
// ─────────────────────────────────────────────────────────────────

#[test]
fn stdlib_comparison_ops() {
    // 10 vs 3: eq=0, ne=1, lt=0, gt=1, lte=0, gte=1.
    // Booleans serialize as 0/1 in interpolated stdout (Value::Bool
    // → 0|1 by the interpolator, not "true"/"false").
    let (stdout, stderr, ok) = run_scenario("comparison_ops");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        first_line(&stdout, "lib/cmp "),
        "lib/cmp eq=0 ne=1 lt=0 gt=1 lte=0 gte=1"
    );
}

// ─────────────────────────────────────────────────────────────────
// Conversion + formatting
// ─────────────────────────────────────────────────────────────────

#[test]
fn stdlib_conversion_to_f64_to_u64() {
    // 42 → 42.0 → 42 round-trip. The f64 intermediate renders
    // with the explicit `.0` suffix per
    // `Value::F64::to_display_string`; the u64 round-trip
    // strips it back to bare `42`.
    let (stdout, stderr, ok) = run_scenario("conversion_to_f64_to_u64");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        first_line(&stdout, "lib/convert "),
        "lib/convert as_f64=42.0 back_u64=42"
    );
}

#[test]
fn stdlib_format_u64_bases() {
    // 255 in dec=255, hex=0xff, bin=0b11111111. format_u64 includes
    // the base prefix for non-decimal bases (`0x`, `0b`).
    let (stdout, stderr, ok) = run_scenario("format_u64_bases");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        first_line(&stdout, "lib/format "),
        "lib/format dec=255 hex=0xff bin=0b11111111"
    );
}

// ─────────────────────────────────────────────────────────────────
// Hashing
// ─────────────────────────────────────────────────────────────────

#[test]
fn stdlib_hash_chain() {
    // hash(0) twice → equal; hash(0) ≠ hash(1).
    // Bools render as 0/1 in interpolated stdout.
    let (stdout, stderr, ok) = run_scenario("hash_chain");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        first_line(&stdout, "lib/hash "),
        "lib/hash eq=1 same=0 diff=1"
    );
}

// ─────────────────────────────────────────────────────────────────
// String interpolation
// ─────────────────────────────────────────────────────────────────

#[test]
fn stdlib_string_interpolation() {
    let (stdout, stderr, ok) = run_scenario("string_interpolation");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        first_line(&stdout, "lib/strinterp "),
        "lib/strinterp greeting=hello, alice! stringified=alice = 42"
    );
}

// ─────────────────────────────────────────────────────────────────
// Encoding round-trips
// ─────────────────────────────────────────────────────────────────

#[test]
fn stdlib_encoding_hex() {
    // u64_to_bytes writes little-endian: 0xDEADBEEF →
    // [0xef, 0xbe, 0xad, 0xde, 0, 0, 0, 0] → hex "efbeadde00000000".
    let (stdout, stderr, ok) = run_scenario("encoding_hex");
    assert!(ok, "scenario failed: {stderr}");
    let line = first_line(&stdout, "lib/hex ");
    assert!(
        line.contains("h=efbeadde00000000"),
        "hex form unexpected: {line}"
    );
    // Round-trip equality — h and h2 must match.
    let h = line.split("h=").nth(1).unwrap().split(' ').next().unwrap();
    let h2 = line.split("roundtrip_equal=").nth(1).unwrap();
    assert_eq!(h, h2, "hex round-trip mismatch: {line}");
}

#[test]
fn stdlib_encoding_base64() {
    // 0xDEADBEEF as 8-byte BE = [0,0,0,0,0xde,0xad,0xbe,0xef] →
    // "AAAAAN6tvu8=".
    let (stdout, stderr, ok) = run_scenario("encoding_base64");
    assert!(ok, "scenario failed: {stderr}");
    let line = first_line(&stdout, "lib/base64 ");
    let e = line.split("e=").nth(1).unwrap().split(' ').next().unwrap();
    let e2 = line.split("roundtrip_equal=").nth(1).unwrap();
    assert_eq!(e, e2, "base64 round-trip mismatch: {line}");
}

#[test]
fn stdlib_encoding_url() {
    // "hello world & friends" → "hello%20world%20%26%20friends".
    let (stdout, stderr, ok) = run_scenario("encoding_url");
    assert!(ok, "scenario failed: {stderr}");
    let line = first_line(&stdout, "lib/urlenc ");
    assert!(
        line.contains("dec=hello world & friends"),
        "url round-trip didn't restore original: {line}"
    );
    assert!(
        line.contains("%20"),
        "url encoding didn't escape spaces: {line}"
    );
}

// ─────────────────────────────────────────────────────────────────
// Digests
// ─────────────────────────────────────────────────────────────────

#[test]
fn stdlib_digest_sha256() {
    // sha256 of the 8 zero bytes is a fixed value.
    let (stdout, stderr, ok) = run_scenario("digest_sha256");
    assert!(ok, "scenario failed: {stderr}");
    let line = first_line(&stdout, "lib/sha256 ");
    assert_eq!(
        line,
        "lib/sha256 hex=af5570f5a1810b7af78caf4bc70a660f0df51e42baf91d4de5b2328de0e83dfc"
    );
}

#[test]
fn stdlib_digest_md5() {
    // md5 of 8 zero bytes.
    let (stdout, stderr, ok) = run_scenario("digest_md5");
    assert!(ok, "scenario failed: {stderr}");
    let line = first_line(&stdout, "lib/md5 ");
    assert_eq!(line, "lib/md5 hex=7dea362b3fac8e00956a4952a3d4f474");
}

// ─────────────────────────────────────────────────────────────────
// Probability + distributions
// ─────────────────────────────────────────────────────────────────

#[test]
fn stdlib_probability_unit_interval() {
    // unit_interval is in [0.0, 1.0). Bools render as 0/1.
    let (stdout, stderr, ok) = run_scenario("probability_unit_interval");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        first_line(&stdout, "lib/unit_interval "),
        "lib/unit_interval in_unit=1 non_neg=1"
    );
}

#[test]
fn stdlib_distribution_uniform() {
    let (stdout, stderr, ok) = run_scenario("distribution_uniform");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        first_line(&stdout, "lib/dist_uniform "),
        "lib/dist_uniform ge_lo=1 lt_hi=1"
    );
}

#[test]
fn stdlib_fair_coin_flip() {
    // fair_coin yields 0 or 1; `heads | tails` is always true (1).
    let (stdout, stderr, ok) = run_scenario("fair_coin_flip");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        first_line(&stdout, "lib/fair_coin "),
        "lib/fair_coin one_of_two=1"
    );
}

// ─────────────────────────────────────────────────────────────────
// Weighted selection
// ─────────────────────────────────────────────────────────────────

#[test]
fn stdlib_weighted_strings() {
    let (stdout, stderr, ok) = run_scenario("weighted_strings_pick");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        first_line(&stdout, "lib/weighted_strings "),
        "lib/weighted_strings is_known=1"
    );
}

#[test]
fn stdlib_weighted_u64() {
    let (stdout, stderr, ok) = run_scenario("weighted_u64_pick");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        first_line(&stdout, "lib/weighted_u64 "),
        "lib/weighted_u64 is_known=1"
    );
}

// ─────────────────────────────────────────────────────────────────
// Pick / select / blend
// ─────────────────────────────────────────────────────────────────

#[test]
fn stdlib_pick_select_blend() {
    // select(1, 100, 200) → 100 (cond != 0 selects if_true).
    // blend(10, 20, 0.5) → 15 (50/50 mix of two u64 wires).
    let (stdout, stderr, ok) = run_scenario("pick_select_blend");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(first_line(&stdout, "lib/pick "), "lib/pick sel=100 bl=15");
}

// ─────────────────────────────────────────────────────────────────
// Lerp / quantize / clamp
// ─────────────────────────────────────────────────────────────────

#[test]
fn stdlib_lerp_and_clamp() {
    // lerp(0.5, 0, 100) = 50.
    // clamp_f64: -5 → 0, 5 → 5, 15 → 10.
    // quantize(0.37, 0.1) ≈ 0.4.
    let (stdout, stderr, ok) = run_scenario("lerp_and_clamp");
    assert!(ok, "scenario failed: {stderr}");
    let line = first_line(&stdout, "lib/lerp ");
    assert!(line.contains("mid=50"), "lerp midpoint: {line}");
    assert!(line.contains("below=0"), "clamp below: {line}");
    assert!(line.contains("inside=5"), "clamp inside: {line}");
    assert!(line.contains("above=10"), "clamp above: {line}");
}

// ─────────────────────────────────────────────────────────────────
// Noise
// ─────────────────────────────────────────────────────────────────

#[test]
fn stdlib_noise_perlin_2d() {
    let (stdout, stderr, ok) = run_scenario("noise_perlin_2d");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(first_line(&stdout, "lib/perlin "), "lib/perlin in_range=1");
}

// ─────────────────────────────────────────────────────────────────
// Date / time
// ─────────────────────────────────────────────────────────────────

#[test]
fn stdlib_date_components() {
    // Epoch 0 = 1970-01-01T00:00:00.000 UTC.
    let (stdout, stderr, ok) = run_scenario("date_components");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        first_line(&stdout, "lib/date "),
        "lib/date y=1970 mo=1 d=1 h=0 mi=0 s=0 ms=0"
    );
}

// ─────────────────────────────────────────────────────────────────
// JSON
// ─────────────────────────────────────────────────────────────────

#[test]
fn stdlib_json_round_trip() {
    // to_json(42) → JSON number 42; json_to_str renders to "42".
    let (stdout, stderr, ok) = run_scenario("json_round_trip");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(first_line(&stdout, "lib/json "), "lib/json s=42");
}

// ─────────────────────────────────────────────────────────────────
// Regex
// ─────────────────────────────────────────────────────────────────

#[test]
fn stdlib_regex_match_and_replace() {
    // "abc123def" matched against "[0-9]+" → "123" or "true".
    // replaced → "abc###def".
    let (stdout, stderr, ok) = run_scenario("regex_match_and_replace");
    assert!(ok, "scenario failed: {stderr}");
    let line = first_line(&stdout, "lib/regex ");
    assert!(
        line.contains("replaced=abc###def"),
        "regex_replace did not produce abc###def: {line}"
    );
}
