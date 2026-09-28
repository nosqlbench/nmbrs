// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Magnitude-suffix-aware numeric coercion for workload parameters.
//!
//! A workload param declares its TYPE through the inferred type of its
//! default value (`max_size: 1000000` → u64). The declared default is
//! the source of truth: when a CLI override supplies that param, the
//! override is coerced to the default's numeric type rather than being
//! allowed to flip it to a string. The coercion understands magnitude
//! suffixes, so `max_size=10m` means ten million:
//!
//! - decimal (powers of 1000): `k M G B T P E` (`B` = billion = `G`),
//! - binary  (powers of 1024): `Ki Mi Gi Ti Pi Ei`.
//!
//! Suffixes are case-insensitive. A value that is already a plain
//! number, or whose declared default is non-numeric, is returned
//! unchanged — so a genuine string param (e.g. a glob like `100m`) is
//! never numified.

/// Parse a numeric string with an optional magnitude suffix into `f64`.
/// Returns `None` if the string is not `<number>[suffix]`.
///
/// A plain number (including scientific `1e3`) parses directly and
/// wins, so the float exponent never collides with the `E` (exa)
/// suffix: `1e3` is 1000, while `1E` is 10^18.
pub fn parse_magnitude(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    // Plain number first — keeps `1e3` scientific (1000), not 1 exa.
    if let Ok(v) = t.parse::<f64>() {
        return Some(v);
    }
    let lower = t.to_ascii_lowercase();
    // Binary (`…i`, powers of 1024) before decimal so `mi` is not read
    // as a bare `m`.
    const BINARY: &[(&str, u64)] = &[
        ("ki", 1u64 << 10),
        ("mi", 1u64 << 20),
        ("gi", 1u64 << 30),
        ("ti", 1u64 << 40),
        ("pi", 1u64 << 50),
        ("ei", 1u64 << 60),
    ];
    for (suf, factor) in BINARY {
        if let Some(head) = lower.strip_suffix(suf) {
            return head.trim().parse::<f64>().ok().map(|v| v * *factor as f64);
        }
    }
    // Decimal (powers of 1000). `b` (billion) is an alias for `g`.
    const DECIMAL: &[(&str, f64)] = &[
        ("k", 1e3),
        ("m", 1e6),
        ("g", 1e9),
        ("b", 1e9),
        ("t", 1e12),
        ("p", 1e15),
        ("e", 1e18),
    ];
    for (suf, factor) in DECIMAL {
        if let Some(head) = lower.strip_suffix(suf) {
            return head.trim().parse::<f64>().ok().map(|v| v * factor);
        }
    }
    None
}

/// Render an `f64` so it re-parses as a float (always has a `.` or
/// exponent), so an f64-typed param stays f64 after coercion.
fn emit_f64(v: f64) -> String {
    let s = format!("{v}");
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}

/// Coerce a CLI override to the type inferred from a param's declared
/// default. When the default is numeric (`u64` or `f64`) the override
/// is parsed with [`parse_magnitude`] and re-emitted as a canonical
/// numeric literal (integer for a `u64` default, float for an `f64`
/// default). A non-numeric default, an already-plain-numeric override,
/// or an unparseable override is returned unchanged (so the value, or
/// any downstream type error, names exactly what the operator typed).
pub fn coerce_param_override(default_value: &str, override_value: &str) -> String {
    let dt = default_value.trim();
    let default_is_u64 = dt.parse::<u64>().is_ok();
    let default_is_f64 = !default_is_u64 && dt.parse::<f64>().is_ok();
    if !default_is_u64 && !default_is_f64 {
        return override_value.to_string();
    }
    let ov = override_value.trim();
    // Already a plain number of either shape → leave verbatim.
    if ov.parse::<u64>().is_ok() || ov.parse::<f64>().is_ok() {
        return override_value.to_string();
    }
    match parse_magnitude(ov) {
        Some(v) if default_is_u64 && v.is_finite() && v >= 0.0 => {
            format!("{}", v.round() as u64)
        }
        Some(v) if default_is_f64 && v.is_finite() => emit_f64(v),
        // u64 default with a negative/non-finite magnitude, or an
        // unparseable override: leave as-is.
        _ => override_value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_numbers_parse_directly() {
        assert_eq!(parse_magnitude("1000000"), Some(1_000_000.0));
        assert_eq!(parse_magnitude("1e3"), Some(1000.0)); // scientific, not exa
        assert_eq!(parse_magnitude("3.14"), Some(3.14));
    }

    #[test]
    fn decimal_suffixes() {
        assert_eq!(parse_magnitude("10m"), Some(10_000_000.0));
        assert_eq!(parse_magnitude("10M"), Some(10_000_000.0));
        assert_eq!(parse_magnitude("5k"), Some(5_000.0));
        assert_eq!(parse_magnitude("2b"), Some(2_000_000_000.0));
        assert_eq!(parse_magnitude("1.5g"), Some(1_500_000_000.0));
        assert_eq!(parse_magnitude("1E"), Some(1e18));
    }

    #[test]
    fn binary_suffixes() {
        assert_eq!(parse_magnitude("4Ki"), Some(4096.0));
        assert_eq!(parse_magnitude("10mi"), Some(10.0 * (1u64 << 20) as f64));
        assert_eq!(parse_magnitude("1Gi"), Some((1u64 << 30) as f64));
        // binary beats decimal: `mi` is mebi, not a bare mega.
        assert_ne!(parse_magnitude("1mi"), parse_magnitude("1m"));
    }

    #[test]
    fn non_numbers_are_none() {
        assert_eq!(parse_magnitude("abc"), None);
        assert_eq!(parse_magnitude(""), None);
        assert_eq!(parse_magnitude("m"), None); // suffix with no number
    }

    #[test]
    fn coerce_anchors_to_numeric_default() {
        // The motivating case: u64 default, suffixed override.
        assert_eq!(coerce_param_override("1000000", "10m"), "10000000");
        // Plain numeric override passes through unchanged.
        assert_eq!(coerce_param_override("1000000", "10000000"), "10000000");
        // Binary suffix on a u64 default.
        assert_eq!(coerce_param_override("1024", "4Ki"), "4096");
        // f64 default stays float-shaped.
        assert_eq!(coerce_param_override("3.5", "1m"), "1000000.0");
    }

    #[test]
    fn coerce_leaves_string_params_alone() {
        // Non-numeric default → the override is a genuine string (e.g. a
        // glob over profiles named `100m` / `100mi`), never numified.
        assert_eq!(coerce_param_override("*", "100m"), "100m");
        assert_eq!(coerce_param_override("default", "100mi"), "100mi");
        // Numeric default but unparseable override → left as-is.
        assert_eq!(coerce_param_override("1000000", "lots"), "lots");
    }
}
