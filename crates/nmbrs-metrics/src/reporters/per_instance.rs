// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Per-instance metric snapshot reporter — appends one JSONL
//! record per snapshot tick to a separate file for each
//! `(metric_name, labels)` combination.
//!
//! ## Layout
//!
//! All files live in a single configured directory. Filenames
//! encode the metric name and every label pair:
//!
//! ```text
//! <name>__<key1>_<value1>__<key2>_<value2>.jsonl
//! ```
//!
//! A metric instance with no labels writes to `<name>.jsonl`.
//! Characters that aren't `[A-Za-z0-9_-]` get replaced with
//! `_` so the path stays portable across filesystems and
//! shell-safe.
//!
//! ## Record format
//!
//! Each line is one JSON object with `ts` (ms since epoch),
//! `name`, `labels`, `type`, and a type-specific value
//! payload. Distinct record shapes per metric type are
//! distinguished by the `type` discriminator so a downstream
//! consumer can parse without per-file metadata.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::labels::Labels;
use crate::scheduler::Reporter;
use crate::snapshot::{BucketBound, MetricSet, MetricValue};

/// Per-instance JSONL reporter.
///
/// **No file-handle cache.** The previous implementation kept
/// every `(metric_name, labels)` combination's `File` alive in a
/// `HashMap` for the entire run, which monotonically grew the
/// open-FD count — for workloads with many distinct label
/// tuples (e.g. `phase × profile × optimize_for × k × r`), the
/// process would exhaust its `ulimit -n`. Each `report()` call
/// now opens, writes, and closes the target file per record.
/// At the 30s cadence the metrics scheduler runs this on, the
/// open() cost is dwarfed by the rest of the snapshot path.
pub struct PerInstanceReporter {
    dir: PathBuf,
}

impl PerInstanceReporter {
    /// Construct a reporter rooted at `dir`. The directory
    /// is created if missing; an existing directory is
    /// reused so consecutive sessions can co-locate output
    /// when desired.
    pub fn new(dir: impl AsRef<Path>) -> Result<Self, String> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)
            .map_err(|e| format!("create per-instance metrics dir {:?}: {e}", dir))?;
        Ok(Self { dir })
    }
}

impl Reporter for PerInstanceReporter {
    fn report(&mut self, snapshot: &MetricSet) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        for family in snapshot.families() {
            let name = family.name();
            for metric in family.metrics() {
                let labels = metric.labels();
                let Some(point) = metric.point() else {
                    continue;
                };
                let line = render_record(now_ms, name, labels, point.value());
                let key = instance_filename(name, labels);
                let path = self.dir.join(format!("{key}.jsonl"));
                // Open-write-close per record. The File is
                // dropped at the end of the block, closing
                // the FD immediately — bounded resource use
                // regardless of how many distinct instances
                // the workload produces.
                let result = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .and_then(|mut f| writeln!(f, "{line}"));
                if let Err(e) = result {
                    crate::diag::warn(&format!(
                        "warning: per-instance metrics write failed for {key}: {e}"
                    ));
                }
            }
        }
    }

    fn flush(&mut self) {
        // No cache, nothing to flush. Each record's open-write-
        // close path already commits before the FD is dropped.
    }
}

/// Build the safe filename stem for one metric instance.
/// `<name>__<k1>_<v1>__<k2>_<v2>` — label pairs are kept in
/// declaration order so consecutive snapshots write to the
/// same file. Sanitisation runs per token so a `/` or `,` in
/// a label value can't escape the metrics directory.
fn instance_filename(name: &str, labels: &Labels) -> String {
    let mut out = sanitize(name);
    for (k, v) in labels.iter() {
        out.push_str("__");
        out.push_str(&sanitize(k));
        out.push('_');
        out.push_str(&sanitize(v));
    }
    out
}

/// Replace anything outside `[A-Za-z0-9_-]` with `_`. Empty
/// input becomes `_` so the resulting filename always has a
/// non-empty token between separators.
fn sanitize(s: &str) -> String {
    if s.is_empty() {
        return "_".to_string();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

/// Render one JSON record for the metric's current value.
/// Hand-rolled so this crate stays free of a serde_json
/// dependency; the surface here is small enough that a tiny
/// escape helper covers it.
/// One JSONL record for a metric instance at a tick.
///
/// `pub(crate)` so the single-file metrics log renders byte-identical records to
/// the per-instance files: the two differ only in how records are routed to
/// files, never in their shape, so a consumer can parse either with one reader.
pub(crate) fn render_record(
    now_ms: i64,
    name: &str,
    labels: &Labels,
    value: &MetricValue,
) -> String {
    let mut out = String::with_capacity(128);
    out.push('{');
    write_kv(&mut out, "ts", &now_ms.to_string(), false);
    out.push(',');
    write_kv(&mut out, "name", name, true);
    out.push(',');
    out.push_str("\"labels\":{");
    let mut first = true;
    for (k, v) in labels.iter() {
        if !first {
            out.push(',');
        }
        first = false;
        write_kv(&mut out, k, v, true);
    }
    out.push('}');
    match value {
        MetricValue::Counter(c) => {
            out.push(',');
            write_kv(&mut out, "type", "counter", true);
            out.push(',');
            write_kv(&mut out, "count", &c.cumulative.to_string(), false);
        }
        MetricValue::Gauge(g) => {
            out.push(',');
            write_kv(&mut out, "type", "gauge", true);
            out.push(',');
            write_kv(&mut out, "value", &format_f64(g.value), false);
        }
        MetricValue::Histogram(h) => {
            let r = &h.reservoir;
            out.push(',');
            write_kv(&mut out, "type", "histogram", true);
            out.push(',');
            write_kv(&mut out, "count", &r.len().to_string(), false);
            out.push(',');
            write_kv(&mut out, "min", &format_f64(r.min() as f64), false);
            out.push(',');
            write_kv(&mut out, "max", &format_f64(r.max() as f64), false);
            out.push(',');
            write_kv(&mut out, "mean", &format_f64(r.mean()), false);
            out.push(',');
            write_kv(&mut out, "stddev", &format_f64(r.stdev()), false);
            for (label, q) in &[
                ("p50", 0.50),
                ("p75", 0.75),
                ("p90", 0.90),
                ("p95", 0.95),
                ("p98", 0.98),
                ("p99", 0.99),
                ("p999", 0.999),
            ] {
                out.push(',');
                write_kv(
                    &mut out,
                    label,
                    &format_f64(r.value_at_quantile(*q) as f64),
                    false,
                );
            }
        }
        MetricValue::BucketedHistogram(h) => {
            out.push(',');
            write_kv(&mut out, "type", "bucketed_histogram", true);
            out.push_str(",\"buckets\":[");
            let mut first = true;
            for (bound, count) in &h.buckets {
                if !first {
                    out.push(',');
                }
                first = false;
                let le = match bound {
                    BucketBound::Finite(v) => format_f64(*v as f64),
                    BucketBound::PositiveInfinity => "\"+Inf\"".to_string(),
                };
                out.push_str(&format!("{{\"le\":{le},\"count\":{count}}}"));
            }
            out.push(']');
        }
        MetricValue::Info(_) => {
            out.push(',');
            write_kv(&mut out, "type", "info", true);
            out.push(',');
            write_kv(&mut out, "value", "1", false);
        }
        MetricValue::StateSet(s) => {
            out.push(',');
            write_kv(&mut out, "type", "stateset", true);
            out.push_str(",\"states\":{");
            let mut first = true;
            for (state, active) in &s.states {
                if !first {
                    out.push(',');
                }
                first = false;
                write_kv(
                    &mut out,
                    state,
                    if *active { "true" } else { "false" },
                    false,
                );
            }
            out.push('}');
        }
    }
    out.push('}');
    out
}

pub(crate) fn write_kv(out: &mut String, key: &str, value: &str, quote_value: bool) {
    out.push('"');
    escape_into(out, key);
    out.push_str("\":");
    if quote_value {
        out.push('"');
        escape_into(out, value);
        out.push('"');
    } else {
        out.push_str(value);
    }
}

fn escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

/// Plain JSON-friendly float formatting: NaN / ±Inf serialise
/// as `null` (so a downstream parser doesn't choke), finite
/// values render with `{:?}` so a sensible precision survives
/// without sticking trailing zeros on whole numbers.
fn format_f64(v: f64) -> String {
    if !v.is_finite() {
        return "null".to_string();
    }
    if v == v.trunc() && v.abs() < 1e16 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labels::Labels;
    use crate::snapshot::MetricSet;
    use std::time::{Duration, Instant};

    #[test]
    fn writes_one_file_per_instance() {
        let dir = std::env::temp_dir().join("nb_per_instance_test");
        let _ = fs::remove_dir_all(&dir);
        let mut reporter = PerInstanceReporter::new(&dir).unwrap();

        let mut snap = MetricSet::new(Duration::from_secs(1));
        let ann = Labels::of("phase", "ann");
        let pvs = Labels::of("phase", "pvs");
        snap.insert_counter("ops_total", ann, 42, Instant::now());
        snap.insert_counter("ops_total", pvs, 17, Instant::now());
        reporter.report(&snap);
        reporter.flush();

        let ann_path = dir.join("ops_total__phase_ann.jsonl");
        let pvs_path = dir.join("ops_total__phase_pvs.jsonl");
        assert!(ann_path.exists(), "expected per-instance file for ann");
        assert!(pvs_path.exists(), "expected per-instance file for pvs");

        let ann_line = fs::read_to_string(&ann_path).unwrap();
        assert!(ann_line.contains("\"count\":42"));
        assert!(ann_line.contains("\"phase\":\"ann\""));
        let pvs_line = fs::read_to_string(&pvs_path).unwrap();
        assert!(pvs_line.contains("\"count\":17"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn label_values_sanitised_for_path() {
        let labels = Labels::of("name", "path/with..slashes,and,commas");
        let stem = instance_filename("metric", &labels);
        assert!(!stem.contains('/'));
        assert!(!stem.contains(','));
        assert!(!stem.contains('.'));
        assert!(stem.starts_with("metric__name_"));
    }

    #[test]
    fn appending_preserves_history_across_ticks() {
        let dir = std::env::temp_dir().join("nb_per_instance_append");
        let _ = fs::remove_dir_all(&dir);
        let mut reporter = PerInstanceReporter::new(&dir).unwrap();

        let labels = Labels::of("phase", "ann");
        for n in [1u64, 2, 3] {
            let mut snap = MetricSet::new(Duration::from_secs(1));
            snap.insert_counter("ops_total", labels.clone(), n, Instant::now());
            reporter.report(&snap);
            reporter.flush();
        }
        let body = fs::read_to_string(dir.join("ops_total__phase_ann.jsonl")).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("\"count\":1"));
        assert!(lines[2].contains("\"count\":3"));
        let _ = fs::remove_dir_all(&dir);
    }
}
