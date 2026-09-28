// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Console reporter: human-readable delta rates and percentiles.

use std::io::Write;

use crate::scheduler::Reporter;
use crate::snapshot::{MetricSet, MetricValue};

/// Writes human-readable metrics summaries to a `Write` target.
pub struct ConsoleReporter {
    out: Box<dyn Write + Send>,
    /// Previous counter values for delta rate computation.
    prev_counts: std::collections::HashMap<u64, u64>,
}

impl ConsoleReporter {
    pub fn stdout() -> Self {
        Self {
            out: Box::new(std::io::stdout()),
            prev_counts: Default::default(),
        }
    }

    pub fn stderr() -> Self {
        Self {
            out: Box::new(std::io::stderr()),
            prev_counts: Default::default(),
        }
    }

    pub fn new(out: Box<dyn Write + Send>) -> Self {
        Self {
            out,
            prev_counts: Default::default(),
        }
    }

    fn format_nanos(nanos: u64) -> String {
        if nanos < 1_000 {
            format!("{nanos}ns")
        } else if nanos < 1_000_000 {
            format!("{:.1}µs", nanos as f64 / 1_000.0)
        } else if nanos < 1_000_000_000 {
            format!("{:.1}ms", nanos as f64 / 1_000_000.0)
        } else {
            format!("{:.2}s", nanos as f64 / 1_000_000_000.0)
        }
    }

    /// Write to the output, logging on failure.
    fn emit(&mut self, args: std::fmt::Arguments<'_>) {
        if let Err(e) = self.out.write_fmt(args) {
            crate::diag::warn(&format!("warning: console reporter write failed: {e}"));
        }
    }
}

impl Reporter for ConsoleReporter {
    fn report(&mut self, snapshot: &MetricSet) {
        let interval_secs = snapshot.interval().as_secs_f64();
        if interval_secs <= 0.0 {
            return;
        }

        self.emit(format_args!("\n"));

        // Group (family, metric) pairs by `activity` label.
        let mut by_activity: std::collections::BTreeMap<
            String,
            Vec<(&str, &crate::snapshot::Metric)>,
        > = std::collections::BTreeMap::new();
        for family in snapshot.families() {
            for metric in family.metrics() {
                let activity = metric
                    .labels()
                    .get("activity")
                    .unwrap_or("global")
                    .to_string();
                by_activity
                    .entry(activity)
                    .or_default()
                    .push((family.name(), metric));
            }
        }

        for (activity, entries) in &by_activity {
            self.emit(format_args!(
                "── {activity} ({:.1}s) ──────────────────\n",
                interval_secs
            ));

            for (name, metric) in entries {
                let Some(point) = metric.point() else {
                    continue;
                };
                match point.value() {
                    MetricValue::Counter(c) => {
                        let key = metric.labels().identity_hash();
                        let prev = self.prev_counts.get(&key).copied().unwrap_or(0);
                        let delta = c.cumulative.saturating_sub(prev);
                        let rate = delta as f64 / interval_secs;
                        self.prev_counts.insert(key, c.cumulative);

                        if delta > 0 || c.cumulative > 0 {
                            self.emit(format_args!(
                                "  {name}  count={total}  delta={delta}  rate={rate:.0}/s\n",
                                total = c.cumulative
                            ));
                        }
                    }
                    MetricValue::Histogram(h) => {
                        let obs = h.count;
                        if obs == 0 {
                            continue;
                        }
                        let rate = obs as f64 / interval_secs;

                        self.emit(format_args!("  {name}  count={obs}  rate={rate:.0}/s\n"));

                        let p50 = Self::format_nanos(h.reservoir.value_at_quantile(0.50));
                        let p90 = Self::format_nanos(h.reservoir.value_at_quantile(0.90));
                        let p99 = Self::format_nanos(h.reservoir.value_at_quantile(0.99));
                        let p999 = Self::format_nanos(h.reservoir.value_at_quantile(0.999));
                        let max = Self::format_nanos(h.reservoir.max());

                        self.emit(format_args!(
                            "    p50={p50}  p90={p90}  p99={p99}  p999={p999}  max={max}\n"
                        ));
                    }
                    MetricValue::Gauge(g) => {
                        self.emit(format_args!("  {name}  {value:.2}\n", value = g.value));
                    }
                    MetricValue::BucketedHistogram(h) => {
                        self.emit(format_args!(
                            "  {name}  buckets={n}  count={total}\n",
                            n = h.buckets.len(),
                            total = h.count
                        ));
                    }
                    MetricValue::Info(_) => {
                        let pairs: Vec<String> = metric
                            .labels()
                            .iter()
                            .map(|(k, v)| format!("{k}={v}"))
                            .collect();
                        self.emit(format_args!(
                            "  {name}  info: {{{pairs}}}\n",
                            pairs = pairs.join(", ")
                        ));
                    }
                    MetricValue::StateSet(s) => {
                        let active: Vec<&str> = s
                            .states
                            .iter()
                            .filter(|(_, a)| *a)
                            .map(|(n, _)| n.as_str())
                            .collect();
                        self.emit(format_args!(
                            "  {name}  states_active={n} ({list})\n",
                            n = active.len(),
                            list = active.join(",")
                        ));
                    }
                }
            }
        }

        if let Err(e) = self.out.flush() {
            crate::diag::warn(&format!("warning: console reporter flush failed: {e}"));
        }
    }

    fn flush(&mut self) {
        if let Err(e) = self.out.flush() {
            crate::diag::warn(&format!("warning: console reporter flush failed: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_nanos_ranges() {
        assert_eq!(ConsoleReporter::format_nanos(500), "500ns");
        assert_eq!(ConsoleReporter::format_nanos(1_500), "1.5µs");
        assert_eq!(ConsoleReporter::format_nanos(1_500_000), "1.5ms");
        assert_eq!(ConsoleReporter::format_nanos(1_500_000_000), "1.50s");
    }
}
