// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! OpenMetrics / Prometheus text exposition format renderer.
//!
//! Renders a [`MetricSet`] as Prometheus text format, suitable for
//! pushing to VictoriaMetrics `/api/v1/import/prometheus` or serving
//! via a scrape endpoint.

use crate::labels::Labels;
use crate::snapshot::{MetricSet, MetricValue, QUANTILES};

/// Render a [`MetricSet`] as Prometheus text exposition format.
pub fn render_prometheus_text(snapshot: &MetricSet) -> String {
    let mut out = String::new();
    let interval_secs = snapshot.interval().as_secs_f64().max(0.001);

    for family in snapshot.families() {
        let name = sanitize_name(family.name());
        for metric in family.metrics() {
            let label_str = render_labels(metric.labels(), &[]);
            let Some(point) = metric.point() else {
                continue;
            };
            match point.value() {
                MetricValue::Counter(c) => {
                    let name_total = format!("{name}_total");
                    out.push_str(&format!("{name_total}{label_str} {}\n", c.cumulative));
                }
                MetricValue::Gauge(g) => {
                    out.push_str(&format!("{name}{label_str} {}\n", g.value));
                }
                MetricValue::Histogram(h) => {
                    let r = &h.reservoir;
                    let obs = h.count;
                    let sum_seconds = h.sum / 1_000_000_000.0;
                    out.push_str(&format!("{name}_count{label_str} {obs}\n"));
                    out.push_str(&format!("{name}_sum{label_str} {sum_seconds:.6}\n"));

                    for &q in QUANTILES {
                        let val_nanos = r.value_at_quantile(q);
                        let val_seconds = val_nanos as f64 / 1_000_000_000.0;
                        let q_label = if label_str.is_empty() {
                            format!("{{quantile=\"{q}\"}}")
                        } else {
                            let inner = &label_str[1..label_str.len() - 1];
                            format!("{{{inner},quantile=\"{q}\"}}")
                        };
                        out.push_str(&format!("{name}{q_label} {val_seconds:.9}\n"));
                    }

                    let rate = obs as f64 / interval_secs;
                    out.push_str(&format!("{name}_rate{label_str} {rate:.2}\n"));
                }
                MetricValue::BucketedHistogram(h) => {
                    // OpenMetrics §5.3 (Histogram) text format:
                    // `<name>_bucket{le="..."}` per bucket plus
                    // `<name>_sum` and `<name>_count`.
                    for (le, count) in &h.buckets {
                        let le_str = match le {
                            crate::snapshot::BucketBound::Finite(v) => v.to_string(),
                            crate::snapshot::BucketBound::PositiveInfinity => "+Inf".to_string(),
                        };
                        let bucket_label = if label_str.is_empty() {
                            format!("{{le=\"{le_str}\"}}")
                        } else {
                            let inner = &label_str[1..label_str.len() - 1];
                            format!("{{{inner},le=\"{le_str}\"}}")
                        };
                        out.push_str(&format!("{name}_bucket{bucket_label} {count}\n"));
                    }
                    if let Some(s) = h.sum {
                        out.push_str(&format!("{name}_sum{label_str} {s}\n"));
                    }
                    out.push_str(&format!("{name}_count{label_str} {}\n", h.count));
                }
                MetricValue::Info(_) => {
                    // OpenMetrics §5.6 text format: `<name>_info{<labels>} 1`.
                    out.push_str(&format!("{name}_info{label_str} 1\n"));
                }
                MetricValue::StateSet(s) => {
                    // OpenMetrics §5.7 text format: one
                    // sample per state with the family name
                    // as the label key (empty suffix).
                    for (state_name, active) in &s.states {
                        let state_label = if label_str.is_empty() {
                            format!("{{{name}=\"{state_name}\"}}")
                        } else {
                            let inner = &label_str[1..label_str.len() - 1];
                            format!("{{{inner},{name}=\"{state_name}\"}}")
                        };
                        let v = if *active { 1 } else { 0 };
                        out.push_str(&format!("{name}{state_label} {v}\n"));
                    }
                }
            }
        }
    }

    out
}

/// Sanitize a metric name to a valid Prometheus identifier.
fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == ':' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Render labels as Prometheus format, excluding specified keys.
fn render_labels(labels: &Labels, exclude: &[&str]) -> String {
    let pairs: Vec<String> = labels
        .iter()
        .filter(|(k, _)| !exclude.contains(k))
        .map(|(k, v)| format!("{k}=\"{v}\""))
        .collect();
    if pairs.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", pairs.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn render_counter() {
        let mut s = MetricSet::new(Duration::from_secs(1));
        s.insert_counter("ops", Labels::of("activity", "write"), 42, Instant::now());
        let text = render_prometheus_text(&s);
        assert!(text.contains("ops_total{activity=\"write\"} 42"));
    }

    #[test]
    fn render_gauge() {
        let mut s = MetricSet::new(Duration::from_secs(1));
        s.insert_gauge("heap_used", Labels::default(), 1234567.0, Instant::now());
        let text = render_prometheus_text(&s);
        assert!(text.contains("heap_used 1234567"));
    }

    #[test]
    fn render_timer_has_quantiles() {
        let mut h = hdrhistogram::Histogram::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
        for i in 1..=1000 {
            let _ = h.record(i * 1_000_000);
        }
        let mut s = MetricSet::new(Duration::from_secs(5));
        s.insert_histogram("latency", Labels::of("activity", "read"), h, Instant::now());
        let text = render_prometheus_text(&s);
        assert!(text.contains("latency_count"));
        assert!(text.contains("latency_sum"));
        assert!(text.contains("quantile=\"0.99\""));
        assert!(text.contains("latency_rate"));
    }
}
