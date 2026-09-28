// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! CSV metrics reporter: one file per metric, timestamp + stats columns.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::scheduler::Reporter;
use crate::snapshot::{MetricSet, MetricValue};

struct CsvFile {
    file: File,
    header_written: bool,
}

pub struct CsvReporter {
    dir: PathBuf,
    files: HashMap<String, CsvFile>,
}

impl CsvReporter {
    pub fn new(dir: impl AsRef<Path>) -> Result<Self, String> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).map_err(|e| format!("failed to create CSV dir {:?}: {e}", dir))?;
        Ok(Self {
            dir,
            files: HashMap::new(),
        })
    }

    fn ensure_file(&mut self, name: &str, header: &str) -> &mut File {
        let entry = self.files.entry(name.to_string()).or_insert_with(|| {
            let path = self.dir.join(format!("{name}.csv"));
            let file = File::create(&path)
                .unwrap_or_else(|e| panic!("failed to create CSV {:?}: {e}", path));
            CsvFile {
                file,
                header_written: false,
            }
        });
        if !entry.header_written {
            if let Err(e) = writeln!(entry.file, "{header}") {
                crate::diag::warn(&format!("warning: CSV header write failed for {name}: {e}"));
            }
            entry.header_written = true;
        }
        &mut entry.file
    }
}

impl Reporter for CsvReporter {
    fn report(&mut self, snapshot: &MetricSet) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        for family in snapshot.families() {
            let name = family.name().to_string();
            for metric in family.metrics() {
                let Some(point) = metric.point() else {
                    continue;
                };
                match point.value() {
                    MetricValue::Counter(c) => {
                        let file = self.ensure_file(&name, "timestamp_ms,count");
                        if let Err(e) = writeln!(file, "{now_ms},{}", c.cumulative) {
                            crate::diag::warn(&format!(
                                "warning: CSV write failed for {name}: {e}"
                            ));
                        }
                    }
                    MetricValue::Gauge(g) => {
                        let file = self.ensure_file(&name, "timestamp_ms,value");
                        if let Err(e) = writeln!(file, "{now_ms},{}", g.value) {
                            crate::diag::warn(&format!(
                                "warning: CSV write failed for {name}: {e}"
                            ));
                        }
                    }
                    MetricValue::Histogram(h) => {
                        let file = self.ensure_file(
                            &name,
                            "timestamp_ms,count,min,max,mean,stddev,p50,p75,p90,p95,p98,p99,p999",
                        );
                        let r = &h.reservoir;
                        if let Err(e) = writeln!(
                            file,
                            "{},{},{},{},{},{},{},{},{},{},{},{},{}",
                            now_ms,
                            r.len(),
                            r.min(),
                            r.max(),
                            r.mean(),
                            r.stdev(),
                            r.value_at_quantile(0.50),
                            r.value_at_quantile(0.75),
                            r.value_at_quantile(0.90),
                            r.value_at_quantile(0.95),
                            r.value_at_quantile(0.98),
                            r.value_at_quantile(0.99),
                            r.value_at_quantile(0.999),
                        ) {
                            crate::diag::warn(&format!(
                                "warning: CSV write failed for {name}: {e}"
                            ));
                        }
                    }
                    MetricValue::BucketedHistogram(h) => {
                        // One row per bucket: timestamp, le, count.
                        let file = self.ensure_file(&name, "timestamp_ms,le,count");
                        for (le, count) in &h.buckets {
                            let le_str = match le {
                                crate::snapshot::BucketBound::Finite(v) => v.to_string(),
                                crate::snapshot::BucketBound::PositiveInfinity => {
                                    "+Inf".to_string()
                                }
                            };
                            if let Err(e) = writeln!(file, "{now_ms},{le_str},{count}") {
                                crate::diag::warn(&format!(
                                    "warning: CSV write failed for {name}: {e}"
                                ));
                            }
                        }
                    }
                    MetricValue::Info(_) => {
                        // Info metrics carry data in labels;
                        // write a single sentinel row so the
                        // file exists.
                        let file = self.ensure_file(&name, "timestamp_ms,value");
                        if let Err(e) = writeln!(file, "{now_ms},1") {
                            crate::diag::warn(&format!(
                                "warning: CSV write failed for {name}: {e}"
                            ));
                        }
                    }
                    MetricValue::StateSet(s) => {
                        let file = self.ensure_file(&name, "timestamp_ms,state,active");
                        for (state_name, active) in &s.states {
                            let v = if *active { 1 } else { 0 };
                            if let Err(e) = writeln!(file, "{now_ms},{state_name},{v}") {
                                crate::diag::warn(&format!(
                                    "warning: CSV write failed for {name}: {e}"
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    fn flush(&mut self) {
        for cf in self.files.values_mut() {
            if let Err(e) = cf.file.flush() {
                crate::diag::warn(&format!("warning: CSV flush failed: {e}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labels::Labels;
    use std::time::{Duration, Instant};

    #[test]
    fn csv_writes_files() {
        let dir = std::env::temp_dir().join("nb_metrics_csv_test");
        let _ = fs::remove_dir_all(&dir);

        let mut reporter = CsvReporter::new(&dir).unwrap();
        let mut snapshot = MetricSet::new(Duration::from_secs(1));
        snapshot.insert_counter("ops_total", Labels::default(), 42, Instant::now());
        snapshot.insert_gauge("heap_mb", Labels::default(), 256.5, Instant::now());
        reporter.report(&snapshot);
        reporter.flush();

        assert!(dir.join("ops_total.csv").exists());
        assert!(dir.join("heap_mb.csv").exists());

        let content = fs::read_to_string(dir.join("ops_total.csv")).unwrap();
        assert!(content.contains("timestamp_ms,count"));
        assert!(content.contains(",42"));

        let _ = fs::remove_dir_all(&dir);
    }
}
