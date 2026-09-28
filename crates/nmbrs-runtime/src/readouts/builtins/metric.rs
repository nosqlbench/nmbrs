// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `metric` — the parameterised readout that surfaces
//! workload-emphasised metrics (recall, latency, etc.).
//!
//! Push 3 surface only: the readout exists in the registry
//! and renders the context's pre-formatted chip string
//! (`status_metric_chips()`). The glob filtering itself
//! still lives in `ActivityMetrics::collect_status_values`
//! and is driven by the workload's `status_metrics:` field.
//! Push 4 will:
//!
//! - Add a typed `LiveMetricSource` to the context.
//! - Have `metric` accept its glob via `pattern=`
//!   options (or the colon shorthand `metric:recall*`).
//! - Drop the `status_metric_chips()` accessor in favour
//!   of per-metric resolution inside the readout.
//!
//! For now this is a thin shim that exists so the body
//! grammar can reference `metric` without errors.

use crate::lifecycle::SubjectKind;
use crate::readouts::buf::ReadoutBuf;
use crate::readouts::context::ReadoutContext;
use crate::readouts::readout::{ContentMode, Lod, Readout, ReadoutOptions};

pub struct Metric;

impl Readout for Metric {
    fn name(&self) -> &'static str {
        "metric"
    }
    fn accepts(&self) -> &'static [SubjectKind] {
        &[SubjectKind::Phase]
    }

    fn render(
        &self,
        ctx: &dyn ReadoutContext,
        _lod: Lod,
        mode: ContentMode,
        opts: &ReadoutOptions,
        out: &mut dyn ReadoutBuf,
    ) -> usize {
        match mode {
            ContentMode::Value => {
                let chips = ctx.status_metric_chips();
                if chips.is_empty() {
                    return 0;
                }
                // Push 9b: glob-filter the chips by the
                // `pattern=` option (set via the `name:arg`
                // colon-shorthand or `pattern="recall*"`
                // long form). Bare `metric` with no
                // pattern emits the whole chip string —
                // backwards-compatible with Push 3.
                let filtered = match opts.get_str("pattern") {
                    Some(pat) => filter_chips(&chips, pat),
                    None => chips,
                };
                let bytes = filtered.len();
                let _ = out.write_str(&filtered);
                bytes
            }
            ContentMode::Explanation => {
                // Push 7: descriptor — short token
                // explaining what the metric chips show.
                // Only emits when the value path would have
                // something to show, so the overlay matches
                // visibility.
                if ctx.status_metric_chips().is_empty() {
                    return 0;
                }
                let s = match opts.get_str("pattern") {
                    Some(pat) => format!(" (live aggregates matching '{pat}')"),
                    None => " (live aggregates: name:value pairs)".to_string(),
                };
                let _ = out.write_str(&s);
                s.len()
            }
        }
    }
}

/// Filter the pre-rendered chip string (` name:value name:value …`)
/// down to entries whose names match the glob pattern.
/// Each chip is split on `:` to extract the name; the
/// pattern is matched via the same minimal `*`/`?` glob
/// the rest of the engine uses.
///
/// Output format mirrors the input — leading-space-
/// separated chips — so callers concatenate without
/// extra whitespace handling. Empty result when nothing
/// matched.
fn filter_chips(chips: &str, pattern: &str) -> String {
    // Split the chip string on whitespace boundaries.
    // Each chip starts with a leading space (per the
    // ActivityMetrics::collect_status_values format),
    // so we walk word-by-word.
    let mut out = String::with_capacity(chips.len());
    for chip in chips.split_whitespace() {
        let name = chip.split_once(':').map(|(n, _)| n).unwrap_or(chip);
        if glob_match(pattern, name) {
            out.push(' ');
            out.push_str(chip);
        }
    }
    out
}

/// Minimal glob match copy from `nmbrs-runtime::activity`.
/// `*` matches zero-or-more, `?` matches one. Recursive,
/// fine at the chip-name lengths we deal with.
fn glob_match(pattern: &str, candidate: &str) -> bool {
    glob_match_bytes(pattern.as_bytes(), candidate.as_bytes())
}

fn glob_match_bytes(pat: &[u8], s: &[u8]) -> bool {
    match (pat.first(), s.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            glob_match_bytes(&pat[1..], s) || (!s.is_empty() && glob_match_bytes(pat, &s[1..]))
        }
        (Some(b'?'), Some(_)) => glob_match_bytes(&pat[1..], &s[1..]),
        (Some(p), Some(c)) if p == c => glob_match_bytes(&pat[1..], &s[1..]),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readouts::buf::StringBuf;

    struct Ctx {
        chips: String,
    }
    impl ReadoutContext for Ctx {
        fn subject_name(&self) -> &str {
            "x"
        }
        fn subject_seq(&self) -> Option<(usize, usize)> {
            None
        }
        fn subject_labels(&self) -> &str {
            ""
        }
        fn cycles_completed(&self) -> u64 {
            0
        }
        fn cycles_total(&self) -> u64 {
            0
        }
        fn ops_ok(&self) -> u64 {
            0
        }
        fn errors(&self) -> u64 {
            0
        }
        fn retries(&self) -> u64 {
            0
        }
        fn concurrency(&self) -> usize {
            0
        }
        fn elapsed_secs(&self) -> f64 {
            0.0
        }
        fn consumed(&self) -> u64 {
            0
        }
        fn status_metric_chips(&self) -> String {
            self.chips.clone()
        }
        fn depth_indent(&self) -> &str {
            ""
        }
        fn use_color(&self) -> bool {
            false
        }
        fn event(&self) -> crate::lifecycle::EventType {
            crate::lifecycle::EventType::Update
        }
    }

    #[test]
    fn renders_chips_string() {
        let ctx = Ctx {
            chips: " recall_at_10:79.62% latency_p99:1.23ms".into(),
        };
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        let n = Metric.render(
            &ctx,
            Lod::Labeled,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        assert!(n > 0);
        assert_eq!(s, " recall_at_10:79.62% latency_p99:1.23ms");
    }

    #[test]
    fn empty_chips_renders_zero() {
        let ctx = Ctx {
            chips: String::new(),
        };
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        let n = Metric.render(
            &ctx,
            Lod::Labeled,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        assert_eq!(n, 0);
        assert!(s.is_empty());
    }

    #[test]
    fn explanation_mode_emits_descriptor_when_chips_present() {
        let ctx = Ctx {
            chips: " latency_p99:1ms".into(),
        };
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        let n = Metric.render(
            &ctx,
            Lod::Labeled,
            ContentMode::Explanation,
            &ReadoutOptions::new(),
            &mut buf,
        );
        assert!(n > 0);
        assert!(
            s.contains("live aggregates"),
            "expected descriptor, got {s}"
        );
    }

    #[test]
    fn explanation_mode_zero_when_no_chips() {
        let ctx = Ctx {
            chips: String::new(),
        };
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        let n = Metric.render(
            &ctx,
            Lod::Labeled,
            ContentMode::Explanation,
            &ReadoutOptions::new(),
            &mut buf,
        );
        assert_eq!(n, 0);
    }

    #[test]
    fn pattern_filter_keeps_only_matching_chips() {
        let ctx = Ctx {
            chips: " recall_at_10:79.62% latency_p99:1.23ms recall_at_1:42.00% latency_max:5ms"
                .into(),
        };
        let mut opts = ReadoutOptions::new();
        opts.set(
            "pattern",
            super::super::super::OptionValue::Str("recall*".into()),
        );
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        Metric.render(&ctx, Lod::Labeled, ContentMode::Value, &opts, &mut buf);
        assert_eq!(s, " recall_at_10:79.62% recall_at_1:42.00%");
    }

    #[test]
    fn pattern_filter_latency_family() {
        let ctx = Ctx {
            chips: " recall_at_10:79.62% latency_p50:1ms latency_p99:2ms".into(),
        };
        let mut opts = ReadoutOptions::new();
        opts.set(
            "pattern",
            super::super::super::OptionValue::Str("latency*".into()),
        );
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        Metric.render(&ctx, Lod::Labeled, ContentMode::Value, &opts, &mut buf);
        assert_eq!(s, " latency_p50:1ms latency_p99:2ms");
    }

    #[test]
    fn pattern_filter_exact_name_only_one_chip() {
        let ctx = Ctx {
            chips: " recall_at_10:79.62% recall_at_1:42.00%".into(),
        };
        let mut opts = ReadoutOptions::new();
        opts.set(
            "pattern",
            super::super::super::OptionValue::Str("recall_at_1".into()),
        );
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        Metric.render(&ctx, Lod::Labeled, ContentMode::Value, &opts, &mut buf);
        // Only the exact-name chip; recall_at_10 is NOT
        // a match because its name is different.
        assert_eq!(s, " recall_at_1:42.00%");
    }

    #[test]
    fn pattern_with_no_match_returns_zero() {
        let ctx = Ctx {
            chips: " recall_at_10:79.62%".into(),
        };
        let mut opts = ReadoutOptions::new();
        opts.set(
            "pattern",
            super::super::super::OptionValue::Str("nonexistent*".into()),
        );
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        let n = Metric.render(&ctx, Lod::Labeled, ContentMode::Value, &opts, &mut buf);
        assert_eq!(n, 0);
        assert_eq!(s, "");
    }

    #[test]
    fn no_pattern_emits_all_chips() {
        let ctx = Ctx {
            chips: " recall_at_10:79.62% latency_p99:1ms".into(),
        };
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        Metric.render(
            &ctx,
            Lod::Labeled,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        // Bare `metric` (no pattern) preserves the
        // backwards-compatible Push 3 behaviour.
        assert_eq!(s, " recall_at_10:79.62% latency_p99:1ms");
    }
}
