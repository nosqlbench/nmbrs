// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! In-memory summary report built from [`crate::metrics_query::MetricsQuery`].
//!
//! Enumerates every component the cadence reporter has seen, runs a
//! `session_lifetime` query per component, and builds one
//! [`ActivityRow`] per tracked component. Renders the same markdown
//! table as the SQLite reporter — eliminates the SQLite round-trip.

use crate::labels::Labels;
use crate::metrics_query::{MetricsQuery, Selection};
use crate::snapshot::MetricValue;

#[cfg(feature = "sqlite")]
pub use crate::reporters::sqlite::{ReportAggregate, ReportConfig};

/// One row in the summary table — one per distinct component.
pub struct ActivityRow {
    pub activity: String,
    pub cycles: u64,
    pub rate: f64,
    pub latency_p50_ns: Option<f64>,
    pub latency_p99_ns: Option<f64>,
    pub latency_mean_ns: Option<f64>,
    pub gauges: Vec<(String, f64)>,
}

/// Build [`ActivityRow`]s from the unified [`MetricsQuery`] —
/// one row per component the cadence reporter has seen.
///
/// Each component with `cycles_total > 0` and without `nosummary=true`
/// becomes one row. Latency comes from the `cycles_servicetime`
/// histogram. Gauges come from gauge families whose names end in
/// `.mean`.
pub fn rows_from_query(query: &MetricsQuery) -> Vec<ActivityRow> {
    let mut rows = Vec::new();
    let reporter = query.reporter();
    let session_age = reporter.started_at().elapsed().as_secs_f64().max(0.001);

    for labels in reporter.component_labels() {
        // Skip nosummary components
        if labels.get("nosummary") == Some("true") {
            continue;
        }

        // Build a per-component selection by matching every label
        // on the component's `Labels` — session-lifetime merges all
        // tracked components by default, and we want this row's
        // subset only.
        let mut sel = Selection::all();
        for (k, v) in labels.iter() {
            sel = sel.with_label(k, v);
        }
        let snapshot = query.session_lifetime(&sel);

        let activity = format_activity_labels(&labels);

        // Find cycles_total counter (family name == "cycles_total")
        let cycles = snapshot
            .family("cycles_total")
            .and_then(|f| f.metrics().next())
            .and_then(|m| m.point())
            .and_then(|p| match p.value() {
                MetricValue::Counter(c) => Some(c.cumulative),
                _ => None,
            })
            .unwrap_or(0);

        if cycles == 0 {
            continue;
        }

        let rate = cycles as f64 / session_age;

        // Find cycles_servicetime histogram for latency
        let latency = snapshot
            .family("cycles_servicetime")
            .and_then(|f| f.metrics().next())
            .and_then(|m| m.point())
            .and_then(|p| match p.value() {
                MetricValue::Histogram(h) if h.count > 0 => Some((
                    h.reservoir.value_at_quantile(0.50) as f64,
                    h.reservoir.value_at_quantile(0.99) as f64,
                    h.reservoir.mean(),
                )),
                _ => None,
            });

        // Collect gauge values (only `.mean`-suffixed family names)
        let mut gauges: Vec<(String, f64)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for family in snapshot.families() {
            let fname = family.name();
            if !fname.ends_with("_mean") {
                continue;
            }
            let short = fname.strip_suffix("_mean").unwrap_or(fname);
            if seen.contains(short) {
                continue;
            }
            for metric in family.metrics() {
                if let Some(point) = metric.point()
                    && let MetricValue::Gauge(g) = point.value()
                {
                    seen.insert(short.to_string());
                    gauges.push((short.to_string(), g.value));
                    break;
                }
            }
        }

        rows.push(ActivityRow {
            activity,
            cycles,
            rate,
            latency_p50_ns: latency.map(|l| l.0),
            latency_p99_ns: latency.map(|l| l.1),
            latency_mean_ns: latency.map(|l| l.2),
            gauges,
        });
    }

    rows
}

/// Auto-select the time unit suffix so the numeric part has significant digits.
/// Input is nanoseconds (sysref standard).
pub fn format_duration(nanos: f64) -> String {
    if nanos >= 1_000_000_000.0 {
        format!("{:.2}s", nanos / 1_000_000_000.0)
    } else if nanos >= 1_000_000.0 {
        format!("{:.2}ms", nanos / 1_000_000.0)
    } else if nanos >= 1_000.0 {
        format!("{:.2}µs", nanos / 1_000.0)
    } else {
        format!("{:.2}ns", nanos)
    }
}

/// Print a summary report from the unified [`MetricsQuery`].
#[cfg(feature = "sqlite")]
pub fn print_summary_from_query(query: &MetricsQuery, config: &ReportConfig) {
    let row_patterns: Vec<regex::Regex> = config
        .row_filters
        .iter()
        .filter_map(|p| regex::Regex::new(p.trim()).ok())
        .collect();

    let rows = rows_from_query(query);
    if rows.is_empty() {
        return;
    }

    let has_latency = rows.iter().any(|r| r.latency_p50_ns.is_some());
    let mut gauge_names: Vec<String> = Vec::new();
    for row in &rows {
        for (name, _) in &row.gauges {
            if !gauge_names.contains(name) {
                let include = if config.columns.is_empty() {
                    true
                } else {
                    config.columns.iter().any(|p| name.contains(p))
                };
                if include {
                    gauge_names.push(name.clone());
                }
            }
        }
    }

    let mut headers: Vec<String> = vec!["Activity".into(), "Cycles".into(), "Rate".into()];
    if has_latency {
        headers.extend(["p50".into(), "p99".into(), "mean".into()]);
    }
    for name in &gauge_names {
        headers.push(name.clone());
    }

    let mut grid: Vec<Vec<String>> = Vec::new();
    for row in &rows {
        if !row_patterns.is_empty() && !row_patterns.iter().any(|p| p.is_match(&row.activity)) {
            continue;
        }
        grid.push(format_row(row, has_latency, &gauge_names));
    }

    // Compute aggregate rows
    let agg_rows = compute_aggregates(&config.aggregates, &rows, has_latency, &gauge_names);

    if !config.show_details {
        grid.clear();
    }
    if grid.is_empty() && agg_rows.is_empty() {
        return;
    }

    align_activity_column(&mut grid);

    if !agg_rows.is_empty() && !grid.is_empty() {
        let blank: Vec<String> = (0..headers.len()).map(|_| String::new()).collect();
        grid.push(blank);
    }
    grid.extend(agg_rows);

    let ncols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in &grid {
        for (i, cell) in row.iter().enumerate() {
            let w = cell.chars().count();
            if i < ncols && w > widths[i] {
                widths[i] = w;
            }
        }
    }

    println!();
    println!("## Summary");
    println!();

    let mut line = String::from("|");
    for (i, h) in headers.iter().enumerate() {
        line.push_str(&format!(" {:<w$} |", h, w = widths[i]));
    }
    println!("{line}");

    let mut sep = String::from("|");
    for w in &widths {
        sep.push_str(&format!("-{}-|", "-".repeat(*w)));
    }
    println!("{sep}");

    for row in &grid {
        let mut line = String::from("|");
        for (i, cell) in row.iter().enumerate() {
            if i < ncols {
                if i == 0 {
                    line.push_str(&format!(" {:<w$} |", cell, w = widths[i]));
                } else {
                    line.push_str(&format!(" {:>w$} |", cell, w = widths[i]));
                }
            }
        }
        println!("{line}");
    }
    println!();
}

// Only consumed by the sqlite-gated `print_summary_from_query`.
#[cfg(feature = "sqlite")]
fn format_row(row: &ActivityRow, has_latency: bool, gauge_names: &[String]) -> Vec<String> {
    let rate_str = if row.rate > 0.0 {
        format!("{:.0}/s", row.rate)
    } else {
        "-".to_string()
    };
    let mut cells = vec![row.activity.clone(), row.cycles.to_string(), rate_str];
    if has_latency {
        if let (Some(p50), Some(p99), Some(mean)) =
            (row.latency_p50_ns, row.latency_p99_ns, row.latency_mean_ns)
        {
            cells.push(format_duration(p50));
            cells.push(format_duration(p99));
            cells.push(format_duration(mean));
        } else {
            cells.extend(["-".into(), "-".into(), "-".into()]);
        }
    }
    for name in gauge_names {
        let val = row
            .gauges
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| format!("{v:.4}"))
            .unwrap_or_else(|| "-".to_string());
        cells.push(val);
    }
    cells
}

#[cfg(feature = "sqlite")]
fn compute_aggregates(
    aggregates: &[ReportAggregate],
    rows: &[ActivityRow],
    has_latency: bool,
    gauge_names: &[String],
) -> Vec<Vec<String>> {
    let mut agg_rows = Vec::new();
    for agg in aggregates {
        if !agg.group_by.is_empty() {
            // Multi-key grouping form: one aggregate row per
            // distinct value-tuple across `group_by` keys.
            agg_rows.extend(compute_grouped_aggregate(
                agg,
                rows,
                has_latency,
                gauge_names,
            ));
            continue;
        }

        // Existing single-key filter form: one aggregate row.
        let matching: Vec<&ActivityRow> = rows
            .iter()
            .filter(|r| {
                for segment in r.activity.split(", ") {
                    if let Some((k, v)) = segment.split_once('=')
                        && k.trim() == agg.label_key
                        && v.trim().contains(&agg.label_pattern)
                    {
                        return true;
                    }
                }
                false
            })
            .collect();

        let label = format!(
            "**{}({}) over {}~{}**",
            agg.function, agg.column_pattern, agg.label_key, agg.label_pattern,
        );
        let mut cells = vec![label, "-".into(), "-".into()];
        if has_latency {
            cells.extend(["-".into(), "-".into(), "-".into()]);
        }

        for gauge_name in gauge_names {
            if !gauge_name.contains(&agg.column_pattern) {
                cells.push("-".into());
                continue;
            }
            let values: Vec<f64> = matching
                .iter()
                .filter_map(|r| {
                    r.gauges
                        .iter()
                        .find(|(n, _)| n == gauge_name)
                        .map(|(_, v)| *v)
                })
                .collect();
            cells.push(reduce_or_dash(&values, &agg.function));
        }
        agg_rows.push(cells);
    }
    agg_rows
}

/// Compute one aggregate row per distinct value-tuple across
/// `group_by` keys. The label column reads
/// `**mean(recall) [k=10, optimize_for=RECALL]**` so the user
/// can identify which group each row corresponds to.
#[cfg(feature = "sqlite")]
fn compute_grouped_aggregate(
    agg: &ReportAggregate,
    rows: &[ActivityRow],
    has_latency: bool,
    gauge_names: &[String],
) -> Vec<Vec<String>> {
    use std::collections::BTreeMap;
    // Group rows by tuple of values across `agg.group_by`.
    let mut groups: BTreeMap<String, Vec<&ActivityRow>> = BTreeMap::new();
    for row in rows {
        let label_map: std::collections::HashMap<&str, &str> = row
            .activity
            .split(", ")
            .filter_map(|seg| seg.split_once('='))
            .map(|(k, v)| (k.trim(), v.trim()))
            .collect();
        let mut tuple_parts: Vec<String> = Vec::with_capacity(agg.group_by.len());
        let mut all_present = true;
        for key in &agg.group_by {
            match label_map.get(key.as_str()) {
                Some(v) => tuple_parts.push(format!("{key}={v}")),
                None => {
                    all_present = false;
                    break;
                }
            }
        }
        if !all_present {
            continue;
        }
        let tuple_key = tuple_parts.join(", ");
        groups.entry(tuple_key).or_default().push(row);
    }

    let mut out = Vec::new();
    let group_by_header = agg.group_by.join(",");
    for (tuple_key, group_rows) in groups {
        let label = format!(
            "**{}({}) over {} [{tuple_key}]**",
            agg.function, agg.column_pattern, group_by_header,
        );
        let mut cells = vec![label, "-".into(), "-".into()];
        if has_latency {
            cells.extend(["-".into(), "-".into(), "-".into()]);
        }

        for gauge_name in gauge_names {
            if !gauge_name.contains(&agg.column_pattern) {
                cells.push("-".into());
                continue;
            }
            let values: Vec<f64> = group_rows
                .iter()
                .filter_map(|r| {
                    r.gauges
                        .iter()
                        .find(|(n, _)| n == gauge_name)
                        .map(|(_, v)| *v)
                })
                .collect();
            cells.push(reduce_or_dash(&values, &agg.function));
        }
        out.push(cells);
    }
    out
}

#[cfg(feature = "sqlite")]
fn reduce_or_dash(values: &[f64], function: &str) -> String {
    if values.is_empty() {
        return "-".into();
    }
    let result = match function {
        "mean" => values.iter().sum::<f64>() / values.len() as f64,
        "min" => values.iter().cloned().fold(f64::INFINITY, f64::min),
        "max" => values.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        _ => 0.0,
    };
    format!("{result:.4}")
}

/// Format activity labels for display (skip internal labels).
fn format_activity_labels(labels: &Labels) -> String {
    let parts: Vec<String> = labels
        .iter()
        .filter(|(k, _)| !matches!(*k, "session" | "n" | "name" | "nosummary"))
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    parts.join(", ")
}

/// Align label components within the Activity column.
// Only consumed by the sqlite-gated `print_summary_from_query`.
#[cfg(feature = "sqlite")]
fn align_activity_column(grid: &mut [Vec<String>]) {
    if grid.is_empty() {
        return;
    }

    let parsed: Vec<Vec<(String, String)>> = grid
        .iter()
        .map(|row| {
            row[0]
                .split(", ")
                .filter_map(|seg| {
                    let key = seg.split('=').next().unwrap_or("").to_string();
                    if key.is_empty() {
                        None
                    } else {
                        Some((key, seg.to_string()))
                    }
                })
                .collect()
        })
        .collect();

    let mut all_keys: Vec<String> = Vec::new();
    let longest = parsed.iter().max_by_key(|r| r.len());
    if let Some(row) = longest {
        for (key, _) in row {
            if !all_keys.contains(key) {
                all_keys.push(key.clone());
            }
        }
    }
    for row in &parsed {
        for (key, _) in row {
            if !all_keys.contains(key) {
                all_keys.push(key.clone());
            }
        }
    }

    let mut slot_widths: Vec<usize> = vec![0; all_keys.len()];
    for row in &parsed {
        for (i, key) in all_keys.iter().enumerate() {
            if let Some((_, seg)) = row.iter().find(|(k, _)| k == key) {
                let w = seg.chars().count();
                if w > slot_widths[i] {
                    slot_widths[i] = w;
                }
            }
        }
    }

    let sep = ", ";
    let sep_len = sep.len();
    for (row_idx, row) in parsed.iter().enumerate() {
        let mut buf = String::new();
        for (i, key) in all_keys.iter().enumerate() {
            let is_last = i + 1 == all_keys.len();
            let total_w = slot_widths[i] + if is_last { 0 } else { sep_len };
            if let Some((_, seg)) = row.iter().find(|(k, _)| k == key) {
                if is_last {
                    buf.push_str(&format!("{:<w$}", seg, w = slot_widths[i]));
                } else {
                    let with_sep = format!("{}{}", seg, sep);
                    buf.push_str(&format!("{:<w$}", with_sep, w = total_w));
                }
            } else {
                buf.push_str(&" ".repeat(total_w));
            }
        }
        grid[row_idx][0] = buf.trim_end().to_string();
    }
}
