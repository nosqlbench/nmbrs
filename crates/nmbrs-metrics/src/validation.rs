// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! OpenMetrics 1.0 validation helpers.
//!
//! Closes the validation gaps documented in [SRD-40a §8].
//! Each helper returns `Result<(), ValidationError>` —
//! callers decide whether to fail-fast (strict mode), warn,
//! or silently coerce. The recording paths in
//! [`crate::snapshot`] are deliberately permissive (Rust
//! `String` accepts anything UTF-8); the exposition paths
//! in [`crate::reporters::openmetrics`] are stricter.
//!
//! ## What's checked here
//!
//! - **Metric-name ABNF**: `[A-Za-z_:][A-Za-z0-9_:]*` per
//!   OpenMetrics §4.4 ABNF.
//! - **Label-name ABNF**: `[A-Za-z_][A-Za-z0-9_]*` per
//!   OpenMetrics §4.3 ABNF (no colons; tighter than metric
//!   names).
//! - **Reserved-name detection**: names beginning with `__`
//!   are reserved for OpenMetrics-defined uses (`__name__`,
//!   future spec extensions). User-supplied names hitting
//!   this pattern are rejected.
//! - **Unit-suffix invariant**: per spec §4.4, if a unit is
//!   non-empty it MUST be a suffix of the family name
//!   separated by an underscore.
//! - **Bucket-monotonicity**: per spec §5.3, Histogram
//!   bucket counts are monotonically non-decreasing across
//!   sorted-by-`le` buckets. GaugeHistogram (§5.4) is
//!   exempt — gauge histograms can decrease.
//! - **Exemplar 128-char limit**: per spec §4.7, the
//!   serialized form of an exemplar's LabelSet MUST NOT
//!   exceed 128 UTF-8 characters.
//!
//! [SRD-40a §8]: ../../../docs/SRD/40a_metrics_model.md#8-gap-audit-current-state-2026-05-05

use std::fmt;

use crate::snapshot::{BucketBound, Exemplar};

/// Validation outcome. Carries a kind tag (so the caller
/// can decide if it cares) plus a human-readable message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub kind: ValidationKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationKind {
    /// Metric / label name violates the ABNF.
    InvalidName,
    /// Name uses a `__`-reserved prefix.
    ReservedName,
    /// Unit isn't a suffix of the metric name.
    UnitSuffix,
    /// Histogram bucket counts decrease across buckets.
    NonMonotonic,
    /// Exemplar LabelSet exceeds 128 UTF-8 chars.
    ExemplarTooLong,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {}",
            match self.kind {
                ValidationKind::InvalidName => "invalid name",
                ValidationKind::ReservedName => "reserved name",
                ValidationKind::UnitSuffix => "unit suffix",
                ValidationKind::NonMonotonic => "non-monotonic buckets",
                ValidationKind::ExemplarTooLong => "exemplar label-set too long",
            },
            self.message
        )
    }
}

impl std::error::Error for ValidationError {}

// =====================================================================
// Name ABNF checks
// =====================================================================

/// Per OpenMetrics §4.4 ABNF:
///
/// ```text
/// metricname              = metricname-initial-char 0*metricname-char
/// metricname-initial-char = ALPHA / "_" / ":"
/// metricname-char         = metricname-initial-char / DIGIT
/// ```
///
/// Concretely: `[A-Za-z_:][A-Za-z0-9_:]*`. Empty names are
/// rejected.
pub fn check_metric_name(name: &str) -> Result<(), ValidationError> {
    if name.is_empty() {
        return Err(ValidationError {
            kind: ValidationKind::InvalidName,
            message: "metric name must not be empty".into(),
        });
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !is_metric_initial(first) {
        return Err(ValidationError {
            kind: ValidationKind::InvalidName,
            message: format!(
                "metric name '{name}': first character must be \
                 [A-Za-z_:], got '{first}'"
            ),
        });
    }
    for (i, c) in name.char_indices().skip(first.len_utf8()) {
        if !is_metric_char(c) {
            return Err(ValidationError {
                kind: ValidationKind::InvalidName,
                message: format!(
                    "metric name '{name}': character at byte {i} \
                     ('{c}') is not [A-Za-z0-9_:]"
                ),
            });
        }
    }
    Ok(())
}

/// Per OpenMetrics §4.3 ABNF:
///
/// ```text
/// label-name              = label-name-initial-char *label-name-char
/// label-name-initial-char = ALPHA / "_"
/// label-name-char         = label-name-initial-char / DIGIT
/// ```
///
/// Concretely: `[A-Za-z_][A-Za-z0-9_]*`. **No colons**
/// (tighter than metric names).
pub fn check_label_name(name: &str) -> Result<(), ValidationError> {
    if name.is_empty() {
        return Err(ValidationError {
            kind: ValidationKind::InvalidName,
            message: "label name must not be empty".into(),
        });
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !is_label_initial(first) {
        return Err(ValidationError {
            kind: ValidationKind::InvalidName,
            message: format!(
                "label name '{name}': first character must be \
                 [A-Za-z_], got '{first}'"
            ),
        });
    }
    for (i, c) in name.char_indices().skip(first.len_utf8()) {
        if !is_label_char(c) {
            return Err(ValidationError {
                kind: ValidationKind::InvalidName,
                message: format!(
                    "label name '{name}': character at byte {i} \
                     ('{c}') is not [A-Za-z0-9_]"
                ),
            });
        }
    }
    Ok(())
}

fn is_metric_initial(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == ':'
}
fn is_metric_char(c: char) -> bool {
    is_metric_initial(c) || c.is_ascii_digit()
}
fn is_label_initial(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}
fn is_label_char(c: char) -> bool {
    is_label_initial(c) || c.is_ascii_digit()
}

// =====================================================================
// Reserved-name detection
// =====================================================================

/// Per OpenMetrics §4.3 / §4.4: names starting with `__`
/// are RESERVED for spec-defined uses. Catches user code
/// that accidentally uses `__custom`, `__priv`, etc.
///
/// The reader-side synthetic name `__name__` is exempt —
/// it's a recognised OpenMetrics convention.
pub fn check_reserved_name(name: &str) -> Result<(), ValidationError> {
    if name == "__name__" {
        return Ok(());
    }
    if name.starts_with("__") {
        return Err(ValidationError {
            kind: ValidationKind::ReservedName,
            message: format!(
                "name '{name}' uses the `__`-reserved prefix; \
                 only OpenMetrics-defined names may start with \
                 double underscore"
            ),
        });
    }
    Ok(())
}

// =====================================================================
// Unit-suffix invariant
// =====================================================================

/// Per OpenMetrics §4.4: if a unit is non-empty it MUST be
/// a suffix of the family name separated by an underscore.
/// Example: `process_cpu_seconds_total` with unit
/// `seconds` is valid because `_seconds_` precedes `_total`
/// (the family name without the OpenMetrics-suffix).
///
/// nmbrs's writer stores the family name as-emitted, so the
/// check here is on the bare name (without per-type
/// exposition suffixes). The check passes when:
///
/// - `unit` is `None` or empty (no constraint), OR
/// - `name` contains `_<unit>` either as an actual suffix
///   or as an infix immediately before a known
///   exposition suffix (`_total` for counters, `_count`
///   etc.).
pub fn check_unit_suffix(name: &str, unit: Option<&str>) -> Result<(), ValidationError> {
    let Some(unit) = unit else {
        return Ok(());
    };
    if unit.is_empty() {
        return Ok(());
    }
    let needle = format!("_{unit}");
    // Suffix match.
    if name.ends_with(&needle) {
        return Ok(());
    }
    // Infix match before any known exposition suffix.
    for suffix in [
        "_total", "_created", "_count", "_sum", "_bucket", "_gcount", "_gsum", "_info",
    ] {
        if let Some(stem) = name.strip_suffix(suffix)
            && stem.ends_with(&needle)
        {
            return Ok(());
        }
    }
    Err(ValidationError {
        kind: ValidationKind::UnitSuffix,
        message: format!(
            "unit '{unit}' is not a suffix of metric name '{name}'; \
             OpenMetrics §4.4 requires `_{unit}` to appear before \
             any exposition suffix"
        ),
    })
}

// =====================================================================
// Bucket monotonicity (§5.3)
// =====================================================================

/// Per OpenMetrics §5.3: Histogram bucket counts are
/// **cumulative** and MUST be monotonically non-decreasing
/// across sorted-by-`le` buckets. Returns the first
/// violating index pair on failure.
///
/// GaugeHistogram (§5.4) bucket counts MAY decrease;
/// callers who deal with a GaugeHistogram should not call
/// this.
pub fn check_bucket_monotonicity(buckets: &[(BucketBound, u64)]) -> Result<(), ValidationError> {
    let mut prev: Option<u64> = None;
    for (i, (le, count)) in buckets.iter().enumerate() {
        if let Some(p) = prev
            && *count < p
        {
            let le_text = match le {
                BucketBound::Finite(v) => v.to_string(),
                BucketBound::PositiveInfinity => "+Inf".to_string(),
            };
            return Err(ValidationError {
                kind: ValidationKind::NonMonotonic,
                message: format!(
                    "histogram bucket counts must be non-decreasing \
                     (OpenMetrics §5.3); bucket {i} (le={le_text}) has \
                     count {count} < previous {p}"
                ),
            });
        }
        prev = Some(*count);
    }
    Ok(())
}

// =====================================================================
// Exemplar length limit (§4.7)
// =====================================================================

/// Per OpenMetrics §4.7: the serialized form of an
/// exemplar's LabelSet MUST NOT exceed 128 UTF-8 character
/// code points. The "serialized form" excludes structural
/// punctuation (`",=`) per spec §4.6.1; our check sums the
/// raw character lengths of all key/value strings.
///
/// 128 chars is small; a single trace ID typically fits in
/// 32–64 chars, so workloads adding multiple labels (trace,
/// span, sampler, environment) can run up against this.
/// Exposition layers should drop violating exemplars
/// silently per spec; this helper lets recording-side code
/// emit a diagnostic before the data lands.
pub const EXEMPLAR_LABELSET_MAX_CHARS: usize = 128;

pub fn check_exemplar_length(exemplar: &Exemplar) -> Result<(), ValidationError> {
    let total: usize = exemplar
        .labels
        .iter()
        .map(|(k, v)| k.chars().count() + v.chars().count())
        .sum();
    if total > EXEMPLAR_LABELSET_MAX_CHARS {
        return Err(ValidationError {
            kind: ValidationKind::ExemplarTooLong,
            message: format!(
                "exemplar LabelSet serialized to {total} chars; \
                 OpenMetrics §4.7 caps at {EXEMPLAR_LABELSET_MAX_CHARS}. \
                 Exposition will drop this exemplar."
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Metric name ABNF ──

    #[test]
    fn metric_names_accept_valid_initial_chars() {
        assert!(check_metric_name("foo").is_ok());
        assert!(check_metric_name("_foo").is_ok());
        assert!(check_metric_name(":foo").is_ok());
        assert!(check_metric_name("foo_bar").is_ok());
        assert!(check_metric_name("foo:bar").is_ok());
        assert!(check_metric_name("a1").is_ok());
    }

    #[test]
    fn metric_names_reject_invalid() {
        assert!(check_metric_name("").is_err());
        assert!(check_metric_name("1foo").is_err()); // starts with digit
        assert!(check_metric_name("foo bar").is_err()); // space
        assert!(check_metric_name("foo.bar").is_err()); // dot
        assert!(check_metric_name("foo-bar").is_err()); // hyphen
        assert!(check_metric_name("é").is_err()); // non-ASCII
    }

    // ── Label name ABNF ──

    #[test]
    fn label_names_accept_valid() {
        assert!(check_label_name("foo").is_ok());
        assert!(check_label_name("_foo").is_ok());
        assert!(check_label_name("foo_bar").is_ok());
        assert!(check_label_name("a1").is_ok());
    }

    #[test]
    fn label_names_reject_colon() {
        // Tighter than metric names.
        let err = check_label_name("foo:bar").unwrap_err();
        assert_eq!(err.kind, ValidationKind::InvalidName);
    }

    #[test]
    fn label_names_reject_other_invalid() {
        assert!(check_label_name("").is_err());
        assert!(check_label_name("1foo").is_err());
        assert!(check_label_name("foo bar").is_err());
    }

    // ── Reserved names ──

    #[test]
    fn reserved_check_allows_name_label_synthetic() {
        assert!(check_reserved_name("__name__").is_ok());
    }

    #[test]
    fn reserved_check_rejects_double_underscore_prefix() {
        let err = check_reserved_name("__custom").unwrap_err();
        assert_eq!(err.kind, ValidationKind::ReservedName);
        assert!(check_reserved_name("__priv").is_err());
        assert!(check_reserved_name("__").is_err());
    }

    #[test]
    fn reserved_check_allows_single_underscore_prefix() {
        assert!(check_reserved_name("_foo").is_ok());
    }

    // ── Unit suffix ──

    #[test]
    fn unit_suffix_passes_when_name_ends_with_unit() {
        assert!(check_unit_suffix("memory_bytes", Some("bytes")).is_ok());
        assert!(check_unit_suffix("uptime_seconds", Some("seconds")).is_ok());
    }

    #[test]
    fn unit_suffix_passes_when_unit_precedes_known_exposition_suffix() {
        assert!(check_unit_suffix("process_cpu_seconds_total", Some("seconds")).is_ok());
        assert!(check_unit_suffix("requests_bytes_count", Some("bytes")).is_ok());
        assert!(check_unit_suffix("latency_seconds_bucket", Some("seconds")).is_ok());
    }

    #[test]
    fn unit_suffix_rejects_when_unit_not_suffix() {
        assert!(check_unit_suffix("memory_used", Some("bytes")).is_err());
        assert!(check_unit_suffix("foo", Some("seconds")).is_err());
    }

    #[test]
    fn unit_suffix_passes_for_empty_unit() {
        assert!(check_unit_suffix("anything", None).is_ok());
        assert!(check_unit_suffix("anything", Some("")).is_ok());
    }

    // ── Bucket monotonicity ──

    #[test]
    fn buckets_monotonic_increasing_passes() {
        let bs = vec![
            (BucketBound::Finite(1), 5),
            (BucketBound::Finite(10), 7),
            (BucketBound::Finite(100), 12),
            (BucketBound::PositiveInfinity, 15),
        ];
        assert!(check_bucket_monotonicity(&bs).is_ok());
    }

    #[test]
    fn buckets_monotonic_constant_passes() {
        // Equal counts across buckets are fine.
        let bs = vec![
            (BucketBound::Finite(1), 5),
            (BucketBound::Finite(10), 5),
            (BucketBound::PositiveInfinity, 5),
        ];
        assert!(check_bucket_monotonicity(&bs).is_ok());
    }

    #[test]
    fn buckets_decreasing_rejected() {
        let bs = vec![
            (BucketBound::Finite(1), 10),
            (BucketBound::Finite(10), 5),
            (BucketBound::PositiveInfinity, 7),
        ];
        let err = check_bucket_monotonicity(&bs).unwrap_err();
        assert_eq!(err.kind, ValidationKind::NonMonotonic);
        assert!(
            err.message.contains("le=10"),
            "error should name the violating bucket: {err}"
        );
    }

    // ── Exemplar length ──

    #[test]
    fn exemplar_under_limit_passes() {
        let ex = Exemplar::new(crate::labels::Labels::of("trace_id", "abc123"), 42.0);
        assert!(check_exemplar_length(&ex).is_ok());
    }

    #[test]
    fn exemplar_over_limit_rejected() {
        // Build a label whose serialized chars exceed 128.
        let big_value = "x".repeat(150);
        let ex = Exemplar::new(crate::labels::Labels::of("trace_id", big_value), 42.0);
        let err = check_exemplar_length(&ex).unwrap_err();
        assert_eq!(err.kind, ValidationKind::ExemplarTooLong);
    }

    #[test]
    fn exemplar_at_exact_limit_passes() {
        let v = "x".repeat(EXEMPLAR_LABELSET_MAX_CHARS - "trace_id".len());
        let ex = Exemplar::new(crate::labels::Labels::of("trace_id", v), 42.0);
        assert!(check_exemplar_length(&ex).is_ok());
    }
}
