// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-40b §1 generation-time numeric sanitiser for synthetic
//! metrics. Parses Excel-style hash-pattern format strings
//! (`#.##`, `0.000`) into a [`FormatSpec`] that the
//! `MetricsDispenser` wrapper consults at registration time
//! to derive a round operation applied before each cycle's
//! value is recorded on the instrument.
//!
//! Excel-style only — printf-style (`%3.2f`) is **not**
//! accepted, per SRD-40b's "one syntax avoids two paths"
//! rule. `#` and `0` placeholders are interchangeable for
//! precision purposes (Excel's render-time drop-trailing-zero
//! distinction doesn't apply here — we're rounding the
//! stored value, not rendering a string).

/// Compiled form of an SRD-40b `format:` declaration. The
/// only field that matters at value-sanitiser time is
/// `decimal_places`; the integer-side layout is recorded for
/// diagnostic display but not enforced on the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatSpec {
    /// Number of decimal places to round to. `0` means "round
    /// to integer."
    pub decimal_places: u8,
    /// Number of placeholder characters before the decimal
    /// point. Layout-only; not applied to the value.
    pub integer_places: u8,
}

impl FormatSpec {
    /// Round `value` to this spec's precision. Used by the
    /// MetricsDispenser before recording on the instrument.
    pub fn apply(&self, value: f64) -> f64 {
        let scale = 10f64.powi(self.decimal_places as i32);
        (value * scale).round() / scale
    }
}

/// Parse an Excel-style hash-pattern format string.
///
/// Accepted shapes (`#` and `0` interchangeable):
/// - `"#"` / `"0"` — round to integer, no decimal portion.
/// - `"#.##"` / `"0.00"` — round to 2 decimal places.
/// - `"##.###"` / `"00.000"` — round to 3 decimal places.
/// - `"#."` — equivalent to `"#"` (trailing `.` allowed,
///   zero decimal places).
///
/// Rejects:
/// - Empty strings.
/// - Printf-style (`%3.2f`).
/// - Multiple `.`s.
/// - Non-`#`/`0` characters (no `,`, no `%`, no `e`).
///
/// Returns the compiled [`FormatSpec`].
pub fn parse_format_spec(s: &str) -> Result<FormatSpec, String> {
    if s.is_empty() {
        return Err("empty format string".into());
    }
    if s.starts_with('%') {
        return Err(format!(
            "printf-style '{s}' not accepted; use Excel-style \
             hash patterns like '#.##' or '0.000'"
        ));
    }

    // Validate every character is `#`, `0`, or `.`. One `.` max.
    let mut dot_seen = false;
    for c in s.chars() {
        match c {
            '#' | '0' => {}
            '.' => {
                if dot_seen {
                    return Err(format!("format '{s}': multiple '.' separators"));
                }
                dot_seen = true;
            }
            other => {
                return Err(format!(
                    "format '{s}': unexpected '{other}' \
                 (only '#', '0', '.' accepted)"
                ));
            }
        }
    }

    let (int_part, dec_part) = match s.split_once('.') {
        Some((i, d)) => (i, d),
        None => (s, ""),
    };

    let integer_places = int_part.len() as u8;
    let decimal_places = dec_part.len() as u8;

    // Allow `"."` (no integer placeholders) only if there are
    // decimal placeholders — `""` and `"."` are degenerate.
    if integer_places == 0 && decimal_places == 0 {
        return Err(format!("format '{s}': no placeholders found"));
    }

    Ok(FormatSpec {
        integer_places,
        decimal_places,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_decimal_patterns() {
        assert_eq!(
            parse_format_spec("#.##").unwrap(),
            FormatSpec {
                integer_places: 1,
                decimal_places: 2
            }
        );
        assert_eq!(
            parse_format_spec("##.###").unwrap(),
            FormatSpec {
                integer_places: 2,
                decimal_places: 3
            }
        );
        assert_eq!(
            parse_format_spec("0.000").unwrap(),
            FormatSpec {
                integer_places: 1,
                decimal_places: 3
            }
        );
    }

    #[test]
    fn parse_integer_only() {
        assert_eq!(
            parse_format_spec("#").unwrap(),
            FormatSpec {
                integer_places: 1,
                decimal_places: 0
            }
        );
        assert_eq!(
            parse_format_spec("0").unwrap(),
            FormatSpec {
                integer_places: 1,
                decimal_places: 0
            }
        );
        assert_eq!(
            parse_format_spec("###").unwrap(),
            FormatSpec {
                integer_places: 3,
                decimal_places: 0
            }
        );
    }

    #[test]
    fn hash_and_zero_interchangeable() {
        // SRD-40b: `#` and `0` are interchangeable for
        // precision purposes (Excel's drop-trailing-zero
        // distinction doesn't apply — we're rounding, not
        // rendering).
        assert_eq!(
            parse_format_spec("#.##").unwrap().decimal_places,
            parse_format_spec("0.00").unwrap().decimal_places,
        );
        assert_eq!(
            parse_format_spec("###").unwrap().decimal_places,
            parse_format_spec("000").unwrap().decimal_places,
        );
    }

    #[test]
    fn rejects_printf_style() {
        let err = parse_format_spec("%3.2f").unwrap_err();
        assert!(err.contains("printf-style"));
    }

    #[test]
    fn rejects_unknown_chars() {
        assert!(parse_format_spec("#,###").is_err()); // comma
        assert!(parse_format_spec("0.0%").is_err()); // percent
        assert!(parse_format_spec("0.0e2").is_err()); // sci-notation
        assert!(parse_format_spec("$0.00").is_err()); // currency
    }

    #[test]
    fn rejects_multiple_dots() {
        assert!(parse_format_spec("0.0.0").is_err());
    }

    #[test]
    fn rejects_empty_and_degenerate() {
        assert!(parse_format_spec("").is_err());
        assert!(parse_format_spec(".").is_err());
    }

    #[test]
    fn apply_rounds_to_decimals() {
        let two = parse_format_spec("#.##").unwrap();
        assert_eq!(two.apply(1.0), 1.0);
        assert_eq!(two.apply(1.234), 1.23);
        assert_eq!(two.apply(1.235), 1.24); // round half up
        assert_eq!(two.apply(1.999), 2.0);

        let three = parse_format_spec("0.000").unwrap();
        assert_eq!(three.apply(1.23456), 1.235);
    }

    #[test]
    fn apply_rounds_to_integer() {
        let int = parse_format_spec("#").unwrap();
        assert_eq!(int.apply(1.0), 1.0);
        assert_eq!(int.apply(1.49), 1.0);
        assert_eq!(int.apply(1.51), 2.0);
        // Rust f64::round rounds half AWAY from zero (not
        // half-to-even). -0.5 rounds to -1.0; the test pins
        // that contract so a future libstd change here won't
        // silently shift synthetic-metric storage values.
        assert_eq!(int.apply(-0.5), -1.0);
        assert_eq!(int.apply(0.5), 1.0);
    }

    #[test]
    fn apply_preserves_negative_values() {
        let two = parse_format_spec("#.##").unwrap();
        assert_eq!(two.apply(-1.234), -1.23);
        assert_eq!(two.apply(-1.235), -1.24);
    }
}
