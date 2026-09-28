// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Stage-by-stage trace event router.
//!
//! `--trace=<spec>` (one or more) configures one or more sinks
//! that receive [`crate::observer::LogLevel::Trace`] events
//! emitted via [`crate::observer::trace`]. The spec grammar:
//!
//! ```text
//!   [<filter>:]<path-template>
//!   filter         = `[` <key> `=` <value> `]`
//!   path-template  = literal | SDIR | `[`<key>`]` | temporal-token | ...
//!   temporal-token = YYYY | YY | MM | DD | HH | MI | SS
//! ```
//!
//! Examples:
//!
//! - `SDIR/trace.log` — all trace events appended to one file.
//! - `[optimize_for=latency]:SDIR/latency.trace` — only events
//!   whose labels carry `optimize_for=latency` reach this sink.
//! - `SDIR/[optimize_for]_YYMMDD.trace` — one file per unique
//!   `optimize_for` value, suffixed with the session start
//!   YYMMDD. Files are opened lazily on first matching event.
//!
//! Path tokens:
//!
//! - `SDIR` — the session directory (substituted at init).
//! - `[KEY]` — the event's label value for `KEY`, sanitised
//!   for filename safety (`/`, `\`, NUL, `.` traversal stripped).
//!   Events lacking the referenced key are silently skipped.
//! - `YYYY` / `YY` / `MM` / `DD` / `HH` / `MI` / `SS` — captured
//!   once at router init, so a path with `YYMMDD` is fixed for
//!   the session.
//!
//! Filename safety: `[KEY]` expansion replaces every character
//! outside `[A-Za-z0-9._-]` with `_`. A bare `..` segment after
//! expansion is rejected (route skipped for that event), so a
//! pathological label value can't escape the templated parent.
//!
//! Zero overhead when no `--trace=…` is supplied: the global
//! router is `None`, and [`enabled`] returns false in a single
//! atomic load. Callers should still guard expensive message
//! formatting with `if trace_router::enabled() { … }` for hot
//! paths.
//!
//! TODO(logging-config): replace this and the existing
//! `loglevel=` / `loglevel-retain=` / per-observer min-level
//! plumbing with a unified JSON/YAML logging config (channels,
//! routes, filters, formatters) plus consistent CLI option
//! sugaring. The current CLI surface is minimal-effort and is
//! expected to be subsumed by that pass.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use nmbrs_metrics::labels::Labels;

/// Captured timestamp parts used to expand temporal tokens at
/// route render time. Captured once at [`init`] so a `YYMMDD`
/// in the path produces a single file for the session, not a
/// new file every event.
#[derive(Clone, Debug)]
struct TemporalParts {
    yyyy: String,
    yy: String,
    mm: String,
    dd: String,
    hh: String,
    mi: String,
    ss: String,
}

impl TemporalParts {
    fn now_local() -> Self {
        // Pull wall-clock components without bringing in a date
        // crate. The session module already formats timestamps
        // for log lines — borrow its routine for consistency.
        let ts = crate::session::now_log_timestamp();
        // ts shape: "2026-05-12 14:23:05.123" (or similar). Parse
        // by fixed offsets; tolerant of missing parts via
        // `unwrap_or_default`.
        let take = |start: usize, len: usize| -> String {
            ts.get(start..start + len).unwrap_or("").to_string()
        };
        let yyyy = take(0, 4);
        Self {
            yy: yyyy.get(2..4).unwrap_or("").to_string(),
            yyyy,
            mm: take(5, 2),
            dd: take(8, 2),
            hh: take(11, 2),
            mi: take(14, 2),
            ss: take(17, 2),
        }
    }

    /// Replace literal `YYYY` / `YY` / `MM` / `DD` / `HH` / `MI`
    /// / `SS` tokens with the captured values. Longest tokens
    /// first to avoid `YY` consuming a `YYYY` prefix.
    fn apply(&self, mut s: String) -> String {
        for (tok, val) in [
            ("YYYY", &self.yyyy),
            ("YY", &self.yy),
            ("MM", &self.mm),
            ("DD", &self.dd),
            ("HH", &self.hh),
            ("MI", &self.mi),
            ("SS", &self.ss),
        ] {
            s = s.replace(tok, val);
        }
        s
    }
}

/// A parsed trace spec — optional label filter plus a path
/// template. Built once at router init from each `--trace=…`
/// CLI argument.
#[derive(Debug)]
struct TraceRoute {
    /// Optional `(key, value)` filter. None means "every trace
    /// event matches".
    filter: Option<(String, String)>,
    /// Path template with `SDIR`, `[KEY]`, and temporal tokens
    /// already expanded (temporal tokens substituted at init
    /// time; `SDIR` substituted to the absolute session dir).
    /// Still contains `[KEY]` placeholders to be expanded per
    /// event.
    template: String,
    /// Lazy file-handle cache keyed by the fully-rendered path.
    /// Files are opened append-only on first matching event.
    /// `Mutex` keeps writes serialised per-file (one writer
    /// per path is intrinsic to the file handle anyway).
    handles: Mutex<HashMap<PathBuf, File>>,
}

impl TraceRoute {
    /// Parse one spec string into a `TraceRoute`, applying
    /// `SDIR` and temporal-token substitution to the path
    /// template eagerly.
    fn parse(spec: &str, session_dir: &str, temporal: &TemporalParts) -> Result<Self, String> {
        // Filter? `[k=v]:path` — the colon between `]` and
        // `path` is mandatory to disambiguate from a path that
        // legitimately starts with `[KEY]`.
        let (filter, path_part) = if let Some(rest) = spec.strip_prefix('[') {
            if let Some(close) = rest.find(']') {
                let inside = &rest[..close];
                let after = &rest[close + 1..];
                if let Some((k, v)) = inside.split_once('=') {
                    // Has `=` → label filter.
                    let after = after.strip_prefix(':').ok_or_else(|| {
                        format!(
                            "trace spec: filter '[{inside}]' must be followed by ':path' \
                         (got '{spec}')"
                        )
                    })?;
                    (
                        Some((k.trim().to_string(), v.trim().to_string())),
                        after.to_string(),
                    )
                } else {
                    // No `=` → this is a path-template `[KEY]`
                    // placeholder, not a filter. The spec
                    // starts with a path token.
                    (None, spec.to_string())
                }
            } else {
                return Err(format!("trace spec: unclosed '[' in '{spec}'"));
            }
        } else {
            (None, spec.to_string())
        };

        if path_part.is_empty() {
            return Err(format!("trace spec: empty path in '{spec}'"));
        }
        // Eager substitutions: SDIR + temporal tokens.
        let with_sdir = path_part.replace("SDIR", session_dir);
        let template = temporal.apply(with_sdir);
        Ok(Self {
            filter,
            template,
            handles: Mutex::new(HashMap::new()),
        })
    }

    /// Determine whether this route matches the event's labels.
    fn matches(&self, labels: &Labels) -> bool {
        match &self.filter {
            None => true,
            Some((k, v)) => labels.get(k).is_some_and(|val| val == v.as_str()),
        }
    }

    /// Render the path template against the event's labels.
    /// Returns `None` when the template references a label
    /// the event doesn't carry (event silently skipped).
    fn render(&self, labels: &Labels) -> Option<PathBuf> {
        let mut out = String::with_capacity(self.template.len());
        let bytes = self.template.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'[' {
                let close = bytes[i + 1..].iter().position(|&b| b == b']')?;
                let key = std::str::from_utf8(&bytes[i + 1..i + 1 + close]).ok()?;
                let val = labels.get(key)?;
                out.push_str(&sanitize_path_segment(val));
                i += 1 + close + 1;
            } else {
                out.push(bytes[i] as char);
                i += 1;
            }
        }
        // Reject any segment that decomposes to ".."
        // post-expansion — a pathological label value mustn't
        // escape the templated parent.
        let p = PathBuf::from(&out);
        if p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return None;
        }
        Some(p)
    }

    /// Append one line to the rendered file path. Lazily opens
    /// the file on first occurrence of each rendered path.
    fn write(&self, path: &PathBuf, line: &[u8]) {
        let mut handles = match self.handles.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let file = match handles.get_mut(path) {
            Some(f) => f,
            None => {
                // Best-effort parent-dir creation; first event
                // with a templated path may need a fresh dir.
                if let Some(parent) = path.parent()
                    && !parent.as_os_str().is_empty()
                {
                    let _ = std::fs::create_dir_all(parent);
                }
                match OpenOptions::new().create(true).append(true).open(path) {
                    Ok(f) => {
                        handles.insert(path.clone(), f);
                        handles.get_mut(path).unwrap()
                    }
                    Err(_) => return,
                }
            }
        };
        let _ = file.write_all(line);
    }
}

/// Replace characters outside `[A-Za-z0-9._-]` with `_` so
/// label values can safely be interpolated into path segments
/// without slash escapes or NUL bytes.
fn sanitize_path_segment(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Global router. `None` until [`init`] runs; subsequent calls
/// to [`log`] short-circuit when unset.
static GLOBAL_ROUTER: OnceLock<Option<TraceRouter>> = OnceLock::new();

#[derive(Debug)]
struct TraceRouter {
    routes: Vec<TraceRoute>,
}

/// Initialise the trace router from a list of `--trace=…` specs
/// and the absolute session directory. Called once by the
/// runner after `log_sink::init`. Subsequent calls are no-ops
/// (first init wins, matching the rest of the global logging
/// surface).
///
/// Returns `Ok(n)` with the number of routes installed (`0`
/// when no specs were supplied — the no-op case).
pub fn init(specs: &[String], session_dir: &std::path::Path) -> Result<usize, String> {
    if specs.is_empty() {
        let _ = GLOBAL_ROUTER.set(None);
        return Ok(0);
    }
    let temporal = TemporalParts::now_local();
    let session_dir_str = session_dir.to_string_lossy().to_string();
    let mut routes = Vec::with_capacity(specs.len());
    for spec in specs {
        routes.push(TraceRoute::parse(spec, &session_dir_str, &temporal)?);
    }
    let n = routes.len();
    let _ = GLOBAL_ROUTER.set(Some(TraceRouter { routes }));
    Ok(n)
}

/// True iff the router has at least one route. Hot-path guard
/// for callers wanting to skip expensive message formatting
/// when tracing is off.
pub fn enabled() -> bool {
    matches!(GLOBAL_ROUTER.get(), Some(Some(_)))
}

/// Route a trace event to every matching sink. The line is
/// formatted as `TS TRC <message>\n` to match the existing
/// `session.log` line shape so trace files are visually
/// consistent with the main log.
pub fn log(labels: &Labels, message: &str) {
    let Some(Some(router)) = GLOBAL_ROUTER.get() else {
        return;
    };
    // Pre-format the line once; reuse across all matching
    // sinks. Trailing newline so the sink-side `write_all`
    // produces line-delimited output.
    let ts = crate::session::now_log_timestamp();
    let line = format!("{ts} TRC {message}\n");
    let bytes = line.as_bytes();
    for route in &router.routes {
        if !route.matches(labels) {
            continue;
        }
        let Some(path) = route.render(labels) else {
            continue;
        };
        route.write(&path, bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporal_stub() -> TemporalParts {
        TemporalParts {
            yyyy: "2026".into(),
            yy: "26".into(),
            mm: "05".into(),
            dd: "12".into(),
            hh: "14".into(),
            mi: "23".into(),
            ss: "05".into(),
        }
    }

    #[test]
    fn parse_plain_path() {
        let r = TraceRoute::parse("SDIR/trace.log", "/s", &temporal_stub()).unwrap();
        assert!(r.filter.is_none());
        assert_eq!(r.template, "/s/trace.log");
    }

    #[test]
    fn parse_filter() {
        let r = TraceRoute::parse(
            "[optimize_for=latency]:SDIR/lat.trace",
            "/s",
            &temporal_stub(),
        )
        .unwrap();
        assert_eq!(r.filter, Some(("optimize_for".into(), "latency".into())));
        assert_eq!(r.template, "/s/lat.trace");
    }

    #[test]
    fn parse_template_with_temporal() {
        let r =
            TraceRoute::parse("SDIR/[optimize_for]_YYMMDD.trace", "/s", &temporal_stub()).unwrap();
        assert!(r.filter.is_none());
        assert_eq!(r.template, "/s/[optimize_for]_260512.trace");
    }

    #[test]
    fn render_substitutes_label_value() {
        let r = TraceRoute::parse("SDIR/[optimize_for].trace", "/s", &temporal_stub()).unwrap();
        let labels = Labels::of("optimize_for", "recall");
        let p = r.render(&labels).unwrap();
        assert_eq!(p, PathBuf::from("/s/recall.trace"));
    }

    #[test]
    fn render_skips_missing_label() {
        let r = TraceRoute::parse("SDIR/[missing].trace", "/s", &temporal_stub()).unwrap();
        let labels = Labels::of("other", "v");
        assert!(r.render(&labels).is_none());
    }

    #[test]
    fn render_sanitises_path_separator() {
        let r = TraceRoute::parse("SDIR/[k].trace", "/s", &temporal_stub()).unwrap();
        let labels = Labels::of("k", "evil/../../etc");
        let p = r.render(&labels).unwrap();
        // Separator replaced, parent-dir token broken up:
        // `evil_.._.._etc.trace` — no `..` segment survives.
        assert!(p.to_string_lossy().contains("evil_"));
        assert!(
            !p.components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        );
    }

    #[test]
    fn filter_matches_label() {
        let r = TraceRoute::parse("[k=v]:SDIR/x", "/s", &temporal_stub()).unwrap();
        assert!(r.matches(&Labels::of("k", "v")));
        assert!(!r.matches(&Labels::of("k", "other")));
        assert!(!r.matches(&Labels::of("other", "v")));
    }
}
