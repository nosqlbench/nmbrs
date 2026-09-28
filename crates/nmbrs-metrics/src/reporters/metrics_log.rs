// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Single-file metrics log: every metric instance, every tick, appended as
//! JSONL to one path.
//!
//! ## Why this exists alongside the session database
//!
//! The session SQLite database is always written and remains the system of
//! record — this sink never replaces it, it only duplicates the same coalesced
//! cadence windows into a plain file. That is for **outside observers**: a
//! process that wants to tail metrics, ship them somewhere, or parse them
//! without linking SQLite, opening a database that another process is actively
//! writing in WAL mode, or knowing the schema.
//!
//! ## Relationship to the other file reporters
//!
//! * [`per_instance`](super::per_instance) writes one file per
//!   `(metric, label-tuple)` — good for isolating one series, awkward to follow
//!   as a whole.
//! * [`csv`](super::csv) writes one file per metric with fixed stat columns —
//!   good for a spreadsheet, lossy for label-rich instances.
//! * This writes **one file, one record per line, all instances interleaved in
//!   tick order** — the shape a log consumer expects: open once, follow to the
//!   end, never enumerate paths.
//!
//! Records are rendered by the same
//! [`render_record`](super::per_instance::render_record) the per-instance files
//! use, so the two are parseable by one reader and differ only in routing.
//!
//! ## Durability
//!
//! One handle is held open and each tick is appended then flushed, so a reader
//! tailing the file sees each tick as it lands rather than when a buffer happens
//! to fill. That costs one `write` + `flush` per tick — the same cadence the
//! database is written at, which is coarse (tens of seconds) by construction, so
//! the cost is irrelevant. A failed write warns once per occurrence and is
//! otherwise ignored: a broken observer log must never disturb the run or the
//! database.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::scheduler::Reporter;
use crate::snapshot::MetricSet;

/// Appends every metric instance of every snapshot tick to a single JSONL file.
pub struct MetricsLogReporter {
    path: PathBuf,
    out: BufWriter<File>,
}

impl MetricsLogReporter {
    /// Open (creating, or appending to an existing) log at `path`.
    ///
    /// Appends rather than truncates: a resumed or re-run session adds to the
    /// same observer log instead of destroying what a reader already consumed.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create metrics log dir {parent:?}: {e}"))?;
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open metrics log {path:?}: {e}"))?;
        Ok(Self {
            path,
            out: BufWriter::new(file),
        })
    }

    /// The log's path, for the startup diagnostic that tells an operator where
    /// to point their reader.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Reporter for MetricsLogReporter {
    fn report(&mut self, snapshot: &MetricSet) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        for family in snapshot.families() {
            let name = family.name();
            for metric in family.metrics() {
                let Some(point) = metric.point() else {
                    continue;
                };
                let line = super::per_instance::render_record(
                    now_ms,
                    name,
                    metric.labels(),
                    point.value(),
                );
                if let Err(e) = writeln!(self.out, "{line}") {
                    crate::diag::warn(&format!(
                        "warning: metrics log write failed for {name}: {e}"
                    ));
                    return;
                }
            }
        }
        // Flush per tick so a tailing reader sees ticks as they land.
        if let Err(e) = self.out.flush() {
            crate::diag::warn(&format!("warning: metrics log flush failed: {e}"));
        }
    }

    fn flush(&mut self) {
        if let Err(e) = self.out.flush() {
            crate::diag::warn(&format!("warning: metrics log final flush failed: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labels::Labels;
    use crate::snapshot::MetricSet;
    use std::time::{Duration, Instant};

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "nmbrs-metrics-log-test-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn writes_every_instance_to_one_file_in_tick_order() {
        let path = temp_path("one-file");
        let mut r = MetricsLogReporter::new(&path).unwrap();

        let mut tick1 = MetricSet::new(Duration::from_secs(1));
        tick1.insert_counter("ops_total", Labels::of("phase", "ann"), 42, Instant::now());
        tick1.insert_counter("ops_total", Labels::of("phase", "pvs"), 17, Instant::now());
        r.report(&tick1);

        let mut tick2 = MetricSet::new(Duration::from_secs(1));
        tick2.insert_counter("ops_total", Labels::of("phase", "ann"), 99, Instant::now());
        r.report(&tick2);
        r.flush();

        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        // All instances land in ONE file, interleaved in tick order — the
        // property that distinguishes this from the per-instance reporter.
        assert_eq!(lines.len(), 3, "one record per instance per tick: {body}");
        assert!(lines[0].contains("\"ops_total\""));
        assert!(lines[0].contains("ann") || lines[1].contains("ann"));
        assert!(lines[0].contains("pvs") || lines[1].contains("pvs"));
        assert!(
            lines[2].contains("99"),
            "later tick appended after earlier: {body}"
        );
        // Every line is a standalone JSON object, so a reader can parse
        // line-by-line without buffering the file.
        for l in &lines {
            assert!(
                l.starts_with('{') && l.ends_with('}'),
                "not a JSON record: {l}"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn appends_rather_than_truncating() {
        // A resumed or re-run session must not destroy what a reader already
        // consumed from the observer log.
        let path = temp_path("append");
        {
            let mut r = MetricsLogReporter::new(&path).unwrap();
            let mut t = MetricSet::new(Duration::from_secs(1));
            t.insert_counter("first", Labels::default(), 1, Instant::now());
            r.report(&t);
            r.flush();
        }
        {
            let mut r = MetricsLogReporter::new(&path).unwrap();
            let mut t = MetricSet::new(Duration::from_secs(1));
            t.insert_counter("second", Labels::default(), 2, Instant::now());
            r.report(&t);
            r.flush();
        }
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("first"),
            "earlier records must survive: {body}"
        );
        assert!(body.contains("second"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn creates_missing_parent_directories() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("nmbrs-metrics-log-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("metrics.jsonl");
        assert!(
            MetricsLogReporter::new(&path).is_ok(),
            "should create parent dirs"
        );
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
