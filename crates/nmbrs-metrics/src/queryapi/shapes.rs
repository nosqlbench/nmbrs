// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Query-result and selector shapes for the metrics query API
//! ([`crate::queryapi`]).
//!
//! These are the **native result shapes of the metrics access
//! library** (SRD-86 §"The metric-reader surface"): a query reads a
//! [`Vector`] — multiple [`Series`], each with one or more [`Sample`]
//! points. The MetricsQL engine evaluates *over* these shapes; it owns
//! no result types of its own. Labels are carried as ordered
//! `(key, value)` pairs (the canonical query-result representation that
//! aggregation / `without` / binary-op label matching manipulate),
//! with `__name__` (the metric family name) carried as a label per
//! PromQL convention.

/// One observation: a value at a point in time (Unix epoch ms).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub timestamp_ms: i64,
    pub value: f64,
}

/// One time series: an identifying label set plus its observed
/// samples (ascending by timestamp). `__name__` lives in `labels`.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub labels: Vec<(String, String)>,
    pub samples: Vec<Sample>,
}

/// A vector result: zero or more series, each with one or more sample
/// points. The *content* distinguishes the MetricsQL result shapes —
///
/// - **instant vector** — one sample per series (a value at an instant);
/// - **range vector** — many samples per series (a window of history);
/// - **scalar** — a single label-less series with one sample.
///
/// The shape an accessor promises is asserted by the metricsql
/// projector; the access library returns this one general container.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Vector(pub Vec<Series>);

impl Vector {
    /// Wrap a series list.
    pub fn new(series: Vec<Series>) -> Self {
        Self(series)
    }
    /// The contained series.
    pub fn series(&self) -> &[Series] {
        &self.0
    }
    /// Consume into the series list.
    pub fn into_series(self) -> Vec<Series> {
        self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<Series>> for Vector {
    fn from(series: Vec<Series>) -> Self {
        Self(series)
    }
}

/// Deref to the series slice so a `Vector` reads like the `&[Series]`
/// it wraps (`.len()`, indexing, `.iter()`), without exposing mutation.
impl std::ops::Deref for Vector {
    type Target = [Series];
    fn deref(&self) -> &[Series] {
        &self.0
    }
}

impl IntoIterator for Vector {
    type Item = Series;
    type IntoIter = std::vec::IntoIter<Series>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl FromIterator<Series> for Vector {
    fn from_iter<I: IntoIterator<Item = Series>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// How a [`Matcher`] compares a label's value. Mirrors the four
/// MetricsQL label-filter operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchOp {
    /// `key="v"`
    Eq,
    /// `key!="v"`
    Ne,
    /// `key=~"re"` — anchored full-value regex.
    EqRegex,
    /// `key!~"re"` — negated anchored full-value regex.
    NeRegex,
}

/// A single label matcher in a selector — one MetricsQL label filter.
#[derive(Debug, Clone, PartialEq)]
pub struct Matcher {
    pub label: String,
    pub op: MatchOp,
    pub value: String,
}

impl Matcher {
    /// `label="value"`.
    pub fn eq(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            op: MatchOp::Eq,
            value: value.into(),
        }
    }
    /// `label!="value"`.
    pub fn ne(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            op: MatchOp::Ne,
            value: value.into(),
        }
    }
    /// `label=~"pattern"`.
    pub fn eq_regex(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            op: MatchOp::EqRegex,
            value: value.into(),
        }
    }

    /// Test this matcher against a series label set. A missing label
    /// reads as the empty string; regex ops anchor the pattern to the
    /// full value (`^(?:pat)$`); an uncompilable pattern fails closed
    /// (`EqRegex` → no match) and open (`NeRegex` → match).
    pub fn matches(&self, labels: &[(String, String)]) -> bool {
        let v = labels
            .iter()
            .find(|(k, _)| k == &self.label)
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        match self.op {
            MatchOp::Eq => v == self.value,
            MatchOp::Ne => v != self.value,
            MatchOp::EqRegex => regex_full_match(&self.value, v).unwrap_or(false),
            MatchOp::NeRegex => !regex_full_match(&self.value, v).unwrap_or(true),
        }
    }
}

/// Anchored full-value regex match (`^(?:pat)$`). `None` on an
/// uncompilable pattern.
fn regex_full_match(pattern: &str, value: &str) -> Option<bool> {
    let anchored = format!("^(?:{pattern})$");
    regex::Regex::new(&anchored)
        .ok()
        .map(|re| re.is_match(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn matcher_covers_all_four_ops() {
        let ls = labels(&[("__name__", "errors_total"), ("phase", "saturate")]);
        assert!(Matcher::eq("phase", "saturate").matches(&ls));
        assert!(!Matcher::eq("phase", "rampup").matches(&ls));
        assert!(Matcher::ne("phase", "rampup").matches(&ls));
        assert!(!Matcher::ne("phase", "saturate").matches(&ls));
        assert!(Matcher::eq_regex("phase", "sat.*").matches(&ls));
        assert!(!Matcher::eq_regex("phase", "ramp.*").matches(&ls));
        let ne_re = Matcher {
            label: "phase".into(),
            op: MatchOp::NeRegex,
            value: "ramp.*".into(),
        };
        assert!(ne_re.matches(&ls));
    }

    #[test]
    fn missing_label_is_empty_and_regex_is_anchored() {
        let ls = labels(&[("__name__", "errors_total")]);
        assert!(Matcher::eq("phase", "").matches(&ls));
        assert!(!Matcher::eq("phase", "saturate").matches(&ls));
        let ls2 = labels(&[("phase", "saturate")]);
        // Anchored: "sat" must not match the full value "saturate".
        assert!(!Matcher::eq_regex("phase", "sat").matches(&ls2));
        assert!(Matcher::eq_regex("phase", "saturate").matches(&ls2));
    }
}
