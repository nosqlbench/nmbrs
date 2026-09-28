// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! OpenMetrics / Prometheus text exposition format parser.
//!
//! Inverse of [`render_prometheus_text()`](super::openmetrics::render_prometheus_text).
//! Parses the text format back into a [`MetricSet`] so that metrics
//! can be pushed over HTTP and reconstructed on the receiving side.
//!
//! Timer histograms are reconstructed approximately — the full HDR
//! precision is lost in the text format, but the dashboard only needs
//! observation count and quantile values, which are preserved.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram as HdrHistogram;

use crate::labels::Labels;
use crate::snapshot::MetricSet;

/// Parse Prometheus text exposition format into a [`MetricSet`].
///
/// Recognises the line formats produced by `render_prometheus_text()`:
///
/// - `name_total{labels} value` → Counter
/// - `name{labels} value` → Gauge (unless part of a timer group)
/// - `name_count{labels} value` → Timer observation count
/// - `name_sum{labels} value` → Timer sum (seconds)
/// - `name{labels,quantile="Q"} value` → Timer quantile
/// - `name_rate{labels} value` → skipped (derived gauge)
///
/// Timer metrics are identified by the presence of `_count` / `_sum`
/// suffixes sharing a common base name. Quantile lines are grouped
/// by base name and used to reconstruct an approximate histogram.
pub fn parse_prometheus_text(text: &str) -> MetricSet {
    let mut lines: Vec<ParsedLine> = Vec::new();
    let mut timer_bases: std::collections::HashSet<String> = std::collections::HashSet::new();

    for raw in text.lines() {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        if let Some(parsed) = parse_line(raw) {
            if let LineKind::TimerCount { .. } = &parsed.kind {
                timer_bases.insert(parsed.base_name.clone());
            }
            lines.push(parsed);
        }
    }

    let mut snapshot = MetricSet::new(Duration::from_secs(1));
    let now = Instant::now();

    let mut timer_parts: HashMap<(String, String), TimerAccum> = HashMap::new();

    for line in &lines {
        match &line.kind {
            LineKind::Counter { value } => {
                snapshot.insert_counter(&line.base_name, line.labels.clone(), *value, now);
            }
            LineKind::Gauge { value } => {
                if line.is_rate {
                    continue;
                }
                if timer_bases.contains(&line.base_name) && line.quantile.is_none() {
                    continue;
                }
                if let Some(q) = line.quantile {
                    let key = (line.base_name.clone(), line.label_key.clone());
                    timer_parts
                        .entry(key)
                        .or_default()
                        .quantiles
                        .push((q, *value));
                } else {
                    snapshot.insert_gauge(&line.base_name, line.labels.clone(), *value, now);
                }
            }
            LineKind::TimerCount { value } => {
                let key = (line.base_name.clone(), line.label_key.clone());
                let accum = timer_parts.entry(key).or_default();
                accum.count = *value;
                accum.labels = line.labels.clone();
            }
            LineKind::TimerSum { value } => {
                let key = (line.base_name.clone(), line.label_key.clone());
                timer_parts.entry(key).or_default().sum_seconds = *value;
            }
        }
    }

    for ((base_name, _), accum) in timer_parts {
        let mut histogram =
            HdrHistogram::new_with_bounds(1, 3_600_000_000_000, 3).expect("histogram bounds");

        if accum.quantiles.is_empty() && accum.count > 0 {
            let mean_nanos = (accum.sum_seconds / accum.count as f64 * 1_000_000_000.0) as u64;
            for _ in 0..accum.count.min(10_000) {
                if let Err(e) = histogram.record(mean_nanos.max(1)) {
                    crate::diag::warn(&format!("warning: histogram record failed: {e}"));
                }
            }
        } else {
            let mut qs = accum.quantiles.clone();
            qs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

            let total = accum.count.max(1);
            let mut prev_q = 0.0_f64;
            for (q, val_seconds) in &qs {
                let val_nanos = (*val_seconds * 1_000_000_000.0) as u64;
                let bucket_fraction = q - prev_q;
                let bucket_count = (bucket_fraction * total as f64).round() as u64;
                for _ in 0..bucket_count.clamp(1, 10_000) {
                    if let Err(e) = histogram.record(val_nanos.max(1)) {
                        crate::diag::warn(&format!("warning: histogram record failed: {e}"));
                    }
                }
                prev_q = *q;
            }
        }

        snapshot.insert_histogram(&base_name, accum.labels, histogram, now);
    }

    snapshot
}

// ─── Internal Types ─────────────────────────────────────────

struct TimerAccum {
    count: u64,
    sum_seconds: f64,
    quantiles: Vec<(f64, f64)>, // (quantile, value_seconds)
    labels: Labels,
}

impl Default for TimerAccum {
    fn default() -> Self {
        Self {
            count: 0,
            sum_seconds: 0.0,
            quantiles: Vec::new(),
            labels: Labels::empty(),
        }
    }
}

enum LineKind {
    Counter { value: u64 },
    Gauge { value: f64 },
    TimerCount { value: u64 },
    TimerSum { value: f64 },
}

struct ParsedLine {
    base_name: String,
    labels: Labels,
    label_key: String, // labels serialised for grouping (excludes quantile)
    kind: LineKind,
    quantile: Option<f64>,
    is_rate: bool,
}

// ─── Line Parser ────────────────────────────────────────────

/// Parse a single Prometheus text line into its components.
fn parse_line(line: &str) -> Option<ParsedLine> {
    // Format: metric_name{labels} value
    // Or:     metric_name value
    let (name_and_labels, value_str) = line.rsplit_once(' ')?;

    let (raw_name, labels_str) = if let Some(brace_start) = name_and_labels.find('{') {
        let name = &name_and_labels[..brace_start];
        let labels_inner = name_and_labels[brace_start + 1..].strip_suffix('}')?;
        (name, Some(labels_inner))
    } else {
        (name_and_labels, None)
    };

    // Parse labels, extracting quantile separately.
    let mut labels = Labels::empty();
    let mut quantile: Option<f64> = None;
    let mut label_parts: Vec<String> = Vec::new();

    if let Some(inner) = labels_str {
        for pair in split_label_pairs(inner) {
            let (k, v) = pair
                .split_once('=')
                .map(|(k, v)| (k, v.trim_matches('"')))?;
            if k == "quantile" {
                quantile = v.parse().ok();
            } else {
                labels = labels.with(k, v);
                label_parts.push(format!("{k}={v}"));
            }
        }
    }

    let label_key = label_parts.join(",");

    // Classify the metric line.
    let is_rate = raw_name.ends_with("_rate");
    if is_rate {
        let base = raw_name.strip_suffix("_rate").unwrap().to_string();
        let value: f64 = value_str.parse().ok()?;
        return Some(ParsedLine {
            base_name: base,
            labels,
            label_key,
            kind: LineKind::Gauge { value },
            quantile: None,
            is_rate: true,
        });
    }

    if raw_name.ends_with("_total") {
        let base = raw_name.strip_suffix("_total").unwrap().to_string();
        let value: u64 = value_str.parse::<f64>().ok()? as u64;
        return Some(ParsedLine {
            base_name: base,
            labels,
            label_key,
            kind: LineKind::Counter { value },
            quantile: None,
            is_rate: false,
        });
    }

    if raw_name.ends_with("_count") {
        let base = raw_name.strip_suffix("_count").unwrap().to_string();
        let value: u64 = value_str.parse::<f64>().ok()? as u64;
        // Store labels on the timer accumulator.
        return Some(ParsedLine {
            base_name: base,
            labels,
            label_key,
            kind: LineKind::TimerCount { value },
            quantile: None,
            is_rate: false,
        });
    }

    if raw_name.ends_with("_sum") {
        let base = raw_name.strip_suffix("_sum").unwrap().to_string();
        let value: f64 = value_str.parse().ok()?;
        return Some(ParsedLine {
            base_name: base,
            labels,
            label_key,
            kind: LineKind::TimerSum { value },
            quantile: None,
            is_rate: false,
        });
    }

    // Plain gauge or timer quantile line.
    let value: f64 = value_str.parse().ok()?;
    Some(ParsedLine {
        base_name: raw_name.to_string(),
        labels,
        label_key,
        kind: LineKind::Gauge { value },
        quantile,
        is_rate: false,
    })
}

/// Split comma-separated label pairs, respecting quoted values.
fn split_label_pairs(s: &str) -> Vec<&str> {
    let mut pairs = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;

    for (i, c) in s.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                let part = s[start..i].trim();
                if !part.is_empty() {
                    pairs.push(part);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = s[start..].trim();
    if !last.is_empty() {
        pairs.push(last);
    }
    pairs
}

// ─── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporters::openmetrics::render_prometheus_text;
    use crate::snapshot::MetricValue;

    #[test]
    fn round_trip_counter() {
        let mut s = MetricSet::new(Duration::from_secs(1));
        s.insert_counter("ops", Labels::of("activity", "write"), 42, Instant::now());
        let text = render_prometheus_text(&s);
        let parsed = parse_prometheus_text(&text);

        let f = parsed.family("ops").expect("ops family");
        assert_eq!(f.len(), 1);
        let m = f.metrics().next().unwrap();
        assert_eq!(m.labels().get("activity"), Some("write"));
        match m.point().unwrap().value() {
            MetricValue::Counter(c) => assert_eq!(c.cumulative, 42),
            _ => panic!("expected counter"),
        }
    }

    #[test]
    fn round_trip_gauge() {
        let mut s = MetricSet::new(Duration::from_secs(1));
        s.insert_gauge("heap_used", Labels::default(), 1234567.0, Instant::now());
        let text = render_prometheus_text(&s);
        let parsed = parse_prometheus_text(&text);

        let f = parsed.family("heap_used").expect("heap_used family");
        let m = f.metrics().next().unwrap();
        match m.point().unwrap().value() {
            MetricValue::Gauge(g) => assert!((g.value - 1234567.0).abs() < 1.0),
            _ => panic!("expected gauge"),
        }
    }

    #[test]
    fn round_trip_timer() {
        let mut h = HdrHistogram::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
        for i in 1..=1000 {
            let _ = h.record(i * 1_000_000);
        }
        let original_p99 = h.value_at_quantile(0.99);

        let mut s = MetricSet::new(Duration::from_secs(5));
        s.insert_histogram("latency", Labels::of("activity", "read"), h, Instant::now());
        let text = render_prometheus_text(&s);
        let parsed = parse_prometheus_text(&text);

        let f = parsed.family("latency").expect("latency family");
        let m = f.metrics().next().expect("series");
        assert_eq!(m.labels().get("activity"), Some("read"));
        match m.point().unwrap().value() {
            MetricValue::Histogram(h) => {
                // Reconstruction from quantile lines is inherently
                // approximate — bucket-count rounding may drop one
                // observation. Within 1% is acceptable.
                assert!(
                    h.count >= 990,
                    "count round-trip lost too much: {}",
                    h.count
                );
                let parsed_p99 = h.reservoir.value_at_quantile(0.99);
                let original_seconds = original_p99 as f64 / 1_000_000_000.0;
                let parsed_seconds = parsed_p99 as f64 / 1_000_000_000.0;
                assert!(
                    (original_seconds - parsed_seconds).abs() < 0.01,
                    "p99 mismatch: original={original_seconds:.6}s, parsed={parsed_seconds:.6}s"
                );
            }
            _ => panic!("expected histogram"),
        }
    }

    #[test]
    fn parse_empty_input() {
        let parsed = parse_prometheus_text("");
        assert!(parsed.is_empty());
    }

    #[test]
    fn parse_ignores_comments() {
        let text = "# HELP foo A counter\n# TYPE foo counter\nfoo_total 42\n";
        let parsed = parse_prometheus_text(text);
        assert_eq!(parsed.len(), 1);
    }
}
