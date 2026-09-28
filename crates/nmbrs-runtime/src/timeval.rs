// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Shared parser for user-facing time-valued settings.
//!
//! The convention for time-based settings a user sets (e.g. the CQL `timeout`
//! op field): a value is EITHER
//!
//! - a **duration spec-string with a unit suffix** — `60s`, `500ms`,
//!   `1h30m`, `2.5s` (units `ms`/`s`/`m`/`h`/`d`; compound and fractional
//!   allowed), OR
//! - a **bare number meaning fractional SECONDS** — `60` = 60s, `60.5` =
//!   60.5s (→ 60500 ms), `0.25` = 250 ms.
//!
//! Distinct from [`crate::session::parse_duration`], which is integer-only
//! (whole seconds/minutes/…) and used for the session shelf-life cap. This
//! parser is the convention for millisecond-resolution op/connection timeouts.

/// Parse a user time value to whole milliseconds.
///
/// A bare number is fractional seconds; a suffixed/compound string is a
/// duration (`ms`/`s`/`m`/`h`/`d`). Returns an actionable error for a
/// negative, non-finite, unit-less, or unknown-unit value.
pub fn parse_time_ms(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty time value".into());
    }
    // Bare number → fractional seconds.
    if let Ok(n) = s.parse::<f64>() {
        if !n.is_finite() || n < 0.0 {
            return Err(format!("time value {s:?}: must be a non-negative number"));
        }
        return Ok((n * 1000.0).round() as u64);
    }
    // Suffixed / compound duration (e.g. `1h30m`, `500ms`, `2.5s`).
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut total_ms: f64 = 0.0;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        if i == start {
            return Err(format!(
                "time value {s:?}: expected a number before the unit"
            ));
        }
        let n: f64 = s[start..i]
            .parse()
            .map_err(|e| format!("time value {s:?}: bad number: {e}"))?;
        if !n.is_finite() || n < 0.0 {
            return Err(format!("time value {s:?}: must be non-negative"));
        }
        let unit_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let unit = &s[unit_start..i];
        let mult = match unit {
            "ms" => 1.0,
            "s" => 1_000.0,
            "m" => 60_000.0,
            "h" => 3_600_000.0,
            "d" => 86_400_000.0,
            "" => {
                return Err(format!(
                    "time value {s:?}: missing unit (use ms/s/m/h/d, \
                 or a bare number for seconds)"
                ));
            }
            other => {
                return Err(format!(
                    "time value {s:?}: unknown unit {other:?} (use ms/s/m/h/d)"
                ));
            }
        };
        total_ms += n * mult;
    }
    Ok(total_ms.round() as u64)
}

#[cfg(test)]
mod tests {
    use super::parse_time_ms;

    #[test]
    fn suffix_forms() {
        assert_eq!(parse_time_ms("60s").unwrap(), 60_000);
        assert_eq!(parse_time_ms("500ms").unwrap(), 500);
        assert_eq!(parse_time_ms("2.5s").unwrap(), 2_500);
        assert_eq!(parse_time_ms("1h30m").unwrap(), 5_400_000);
        assert_eq!(parse_time_ms("1d").unwrap(), 86_400_000);
    }

    #[test]
    fn bare_number_is_fractional_seconds() {
        assert_eq!(parse_time_ms("60").unwrap(), 60_000);
        assert_eq!(parse_time_ms("60.5").unwrap(), 60_500);
        assert_eq!(parse_time_ms("0.25").unwrap(), 250);
        assert_eq!(parse_time_ms("0").unwrap(), 0);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_time_ms("abc").is_err());
        assert!(parse_time_ms("10x").is_err());
        assert!(parse_time_ms("").is_err());
        assert!(parse_time_ms("-5").is_err());
        assert!(parse_time_ms("10 s foo").is_err());
    }
}
