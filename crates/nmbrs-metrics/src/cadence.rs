// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Cadence planning: user-declared cadences + auto-intermediate tree
//! synthesis.
//!
//! This module owns the *planning* side of SRD-42 §"Canonical Cadences"
//! and §"Auto-Intermediate Buckets" — the [`Cadences`] type for the
//! user's declared list and the [`CadenceTree`] planner that
//! synthesizes hidden layers and emits the realized tree at INFO.
//!
//! It does NOT own the runtime store of windowed snapshots — that
//! belongs to [`crate::cadence_reporter::CadenceReporter`].

use std::time::Duration;

// =========================================================================
// Cadences — user-declared list
// =========================================================================

/// User-declared set of canonical latency cadences. The windows the
/// user sees in every consumer (TUI panels, summary reports, etc.)
/// are exactly these, in the order they were declared.
#[derive(Clone, Debug)]
pub struct Cadences {
    /// Cadences in user-declared order. Duplicates are folded out on
    /// construction; ordering is preserved so consumers that display
    /// columns-per-cadence line up with the user's mental model.
    ordered: Vec<Duration>,
}

/// Error type for [`Cadences::parse`]. Cadence values that are
/// structurally illegal (empty input, un-parseable tokens) surface
/// here; *semantic* validity against a base interval is deferred to
/// [`CadenceTree::plan`] so the checks can see the actual scheduler
/// base (which may not be 1 s in tests or alternate configurations).
#[derive(Debug, PartialEq, Eq)]
pub enum CadenceParseError {
    /// The input string was empty.
    Empty,
    /// A token couldn't be parsed as a duration.
    BadToken(String),
}

impl std::fmt::Display for CadenceParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty cadence list"),
            Self::BadToken(s) => write!(f, "not a duration: '{s}' (try '10s', '1m', '1h')"),
        }
    }
}

impl std::error::Error for CadenceParseError {}

/// Error returned by [`CadenceTree::plan_validated`] when a declared
/// cadence violates a base-interval invariant.
#[derive(Debug, PartialEq, Eq)]
pub enum CadenceTreeError {
    /// A cadence is smaller than the scheduler's base interval.
    BelowBase { cadence: Duration, base: Duration },
    /// A cadence is not an integer multiple of the base interval.
    NotMultiple { cadence: Duration, base: Duration },
}

impl std::fmt::Display for CadenceTreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BelowBase { cadence, base } => write!(
                f,
                "cadence {cadence:?} is smaller than base interval {base:?}"
            ),
            Self::NotMultiple { cadence, base } => write!(
                f,
                "cadence {cadence:?} is not an integer multiple of base {base:?}"
            ),
        }
    }
}

impl std::error::Error for CadenceTreeError {}

impl Cadences {
    /// Construct from an ordered list. Filters duplicates (first
    /// occurrence wins) and preserves declaration order.
    ///
    /// Semantic validity against a scheduler base interval is
    /// deferred to [`CadenceTree::plan_validated`] — this constructor
    /// accepts any positive `Duration` so tests and alternate
    /// scheduler configurations (sub-second base intervals) work
    /// without a bypass.
    pub fn new(cadences: &[Duration]) -> Result<Self, CadenceParseError> {
        let mut seen = std::collections::HashSet::new();
        let mut ordered = Vec::with_capacity(cadences.len());
        for c in cadences {
            if seen.insert(*c) {
                ordered.push(*c);
            }
        }
        if ordered.is_empty() {
            return Err(CadenceParseError::Empty);
        }
        Ok(Self { ordered })
    }

    /// Default cadences used when the user didn't specify any:
    /// `1s, 10s, 30s, 1m, 5m`.
    ///
    /// The 1s layer gives consumers (TUI, programmatic pulls)
    /// a tight short-term window — `cadence_window(1s)` always
    /// returns data. Cadence-layer overhead is negligible per the
    /// `cadence_layout` bench, so including 1s in defaults is low
    /// cost. `10s` is the default SQLite persistence cadence;
    /// `30s, 1m, 5m` give coarser rollups for summary reports and
    /// long-run trends.
    pub fn defaults() -> Self {
        Self::new(&[
            Duration::from_secs(1),
            Duration::from_secs(10),
            Duration::from_secs(30),
            Duration::from_secs(60),
            Duration::from_secs(300),
        ])
        .expect("static default cadences are valid")
    }

    /// Parse `"10s,1m,10m,10h"` into a cadence list. Whitespace is
    /// ignored; units: `s`, `m`, `h`. See [`CadenceParseError`] for
    /// failure modes.
    pub fn parse(s: &str) -> Result<Self, CadenceParseError> {
        let mut cadences = Vec::new();
        for token in s.split(',') {
            let t = token.trim();
            if t.is_empty() {
                continue;
            }
            cadences.push(parse_duration(t).map_err(|_| CadenceParseError::BadToken(t.into()))?);
        }
        Self::new(&cadences)
    }

    /// Cadences in user-declared order. Hidden-intermediate buckets
    /// introduced by the scheduler (phase 2) are NOT included here —
    /// this iterator only yields what the user asked for.
    pub fn iter(&self) -> impl Iterator<Item = Duration> + '_ {
        self.ordered.iter().copied()
    }

    /// Number of declared cadences.
    pub fn len(&self) -> usize {
        self.ordered.len()
    }

    /// True when no cadences are declared. `Cadences::new` rejects
    /// empty input, so this is always `false` for constructed values.
    pub fn is_empty(&self) -> bool {
        self.ordered.is_empty()
    }

    /// Smallest cadence — the finest granularity the user asked for.
    /// Acts as the effective "now" bucket for consumers that don't
    /// have a non-draining live read.
    pub fn smallest(&self) -> Duration {
        self.ordered.iter().copied().min().unwrap_or_default()
    }

    /// Largest cadence — the coarsest horizon the user asked for.
    pub fn largest(&self) -> Duration {
        self.ordered.iter().copied().max().unwrap_or_default()
    }
}

/// Parse a human-duration string like `10s`, `500ms`, `1m`, `2h`.
/// Plain integers without a unit are interpreted as seconds.
pub fn parse_duration(s: &str) -> Result<Duration, ()> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("ms") {
        return n
            .trim()
            .parse::<u64>()
            .map(Duration::from_millis)
            .map_err(|_| ());
    }
    if let Some(n) = s.strip_suffix('s') {
        return n
            .trim()
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| ());
    }
    if let Some(n) = s.strip_suffix('m') {
        return n
            .trim()
            .parse::<u64>()
            .map(|v| Duration::from_secs(v * 60))
            .map_err(|_| ());
    }
    if let Some(n) = s.strip_suffix('h') {
        return n
            .trim()
            .parse::<u64>()
            .map(|v| Duration::from_secs(v * 3600))
            .map_err(|_| ());
    }
    s.parse::<u64>().map(Duration::from_secs).map_err(|_| ())
}

// =========================================================================
// CadenceTree — auto-intermediate planner (SRD-42 phase 2)
// =========================================================================

/// Default maximum fan-in between adjacent layers in the realized
/// cadence tree.
///
/// ## What max-fan-in means
///
/// Each cadence layer (e.g. 10 s) aggregates samples from the layer
/// below it (e.g. 1 s). The **fan-in** is the integer ratio
/// `next.interval / prev.interval` — how many of the lower-layer
/// windows roll up into one upper-layer window.
///
/// - Declared `[1 s, 10 s]` → fan-in `10:1` (each 10 s window
///   summarizes 10 of the 1 s windows below it).
/// - Declared `[1 s, 5 m]` → fan-in `300:1` — too aggressive. The
///   planner refuses to leave that as the realized layout because
///   a single 5-minute window summarizing 300 of the 1 s windows
///   loses too much accuracy in the upper-layer aggregates.
///
/// ## Why a cap exists
///
/// The accuracy of the upper-layer aggregates (mean / p99 /
/// histogram fan-out) degrades non-linearly as the fan-in grows.
/// At 20:1 the loss is bounded; at 300:1 the rolled-up percentile
/// is effectively a coin flip. Limiting adjacent fan-in to a known
/// ratio keeps the cost-vs-accuracy trade-off predictable.
///
/// ## How the planner enforces it
///
/// When two declared layers have a ratio above `max_fan_in`, the
/// planner synthesizes **hidden intermediate layers** — geometric
/// midpoints that keep every adjacent step within the limit. With
/// `max_fan_in = 20`:
///
/// - Declared `[1 s, 5 m]` (300:1) → realized `[1 s, (20 s), 5 m]`
///   — `20 s` is hidden, the operator never sees it as a column
///   but the upper-layer aggregates use it. Adjacent ratios:
///   `20:1` and `15:1`, both under the cap.
///
/// The synthesized line in the run log surfaces hidden layers in
/// parentheses, with the trailing `/ N` carrying this cap:
///
/// ```text
/// metrics: cadences: [1s, (20s), 5m] / 20
/// ```
///
/// ## Tuning
///
/// 20 is the workload default. Lower values (more intermediate
/// layers) trade storage for accuracy at very wide cadence
/// declarations. Higher values trust the operator that the
/// rolled-up percentile is "good enough." Override via the
/// `latency-fan-in=` CLI / param when SRD-42 needs the knob —
/// currently not exposed (the constant is the single source of
/// truth project-wide).
pub const DEFAULT_MAX_FAN_IN: u32 = 20;

/// One layer in the realized cadence tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CadenceLayer {
    pub interval: Duration,
    /// True when the layer was synthesized by the planner (not
    /// declared by the user). Hidden layers feed accumulation only —
    /// they are never surfaced via [`Cadences::iter`] or counted as
    /// user-visible columns.
    pub hidden: bool,
}

/// The realized cadence tree: user-declared layers plus any hidden
/// intermediates synthesized to keep adjacent fan-in ≤ `max_fan_in`.
///
/// This is the consumer-facing planning result that the scheduler
/// reads to build its chained tree (SRD-42 §Tree Construction).
/// The user-facing `cadences()` view returns only declared layers.
#[derive(Clone, Debug)]
pub struct CadenceTree {
    declared: Cadences,
    layers: Vec<CadenceLayer>,
    max_fan_in: u32,
}

impl CadenceTree {
    /// Plan a tree from user-declared cadences, validating every
    /// cadence against the scheduler's `base_interval`.
    ///
    /// Returns an error when any declared cadence is below the base
    /// or is not an integer multiple of it. Otherwise produces the
    /// realized tree (declared + auto-inserted hidden layers).
    ///
    /// This is a plan-time hard error per SRD-42 §"Constraints" —
    /// validation happens once on tree construction, not on every
    /// tick.
    pub fn plan_validated(
        declared: Cadences,
        max_fan_in: u32,
        base_interval: Duration,
    ) -> Result<Self, CadenceTreeError> {
        for c in declared.iter() {
            if c < base_interval {
                return Err(CadenceTreeError::BelowBase {
                    cadence: c,
                    base: base_interval,
                });
            }
            if base_interval.as_nanos() == 0 || c.as_nanos() % base_interval.as_nanos() != 0 {
                return Err(CadenceTreeError::NotMultiple {
                    cadence: c,
                    base: base_interval,
                });
            }
        }
        Ok(Self::plan(declared, max_fan_in))
    }

    /// Plan a tree from user-declared cadences without a base-interval
    /// check. Adjacent layers are kept within `max_fan_in`:1 by
    /// inserting geometrically-spaced hidden intermediates. Synthesis
    /// is logged at INFO so operators can see the realized layout
    /// from the run log.
    ///
    /// Callers that have a known scheduler base interval should
    /// prefer [`Self::plan_validated`].
    pub fn plan(declared: Cadences, max_fan_in: u32) -> Self {
        let mut sorted: Vec<Duration> = declared.iter().collect();
        sorted.sort_unstable();
        sorted.dedup();

        let declared_set: std::collections::HashSet<Duration> = sorted.iter().copied().collect();

        let mut layers: Vec<Duration> = sorted.clone();
        synthesize_intermediates(&mut layers, max_fan_in);

        let realized: Vec<CadenceLayer> = layers
            .iter()
            .map(|d| CadenceLayer {
                interval: *d,
                hidden: !declared_set.contains(d),
            })
            .collect();

        log_realized_tree(&realized, max_fan_in);

        Self {
            declared,
            layers: realized,
            max_fan_in,
        }
    }

    /// Plan with the default `max_fan_in` ([`DEFAULT_MAX_FAN_IN`]).
    pub fn plan_default(declared: Cadences) -> Self {
        Self::plan(declared, DEFAULT_MAX_FAN_IN)
    }

    /// User-declared cadences in their original declaration order.
    pub fn declared(&self) -> &Cadences {
        &self.declared
    }

    /// All layers (declared + hidden), sorted ascending by interval.
    pub fn layers(&self) -> &[CadenceLayer] {
        &self.layers
    }

    /// Maximum fan-in used during planning.
    pub fn max_fan_in(&self) -> u32 {
        self.max_fan_in
    }

    /// Just the hidden (auto-inserted) layers, ascending.
    pub fn hidden(&self) -> impl Iterator<Item = Duration> + '_ {
        self.layers.iter().filter(|l| l.hidden).map(|l| l.interval)
    }

    /// Route a reporter's preferred interval to the nearest
    /// declared cadence ≥ `preferred`. If no declared cadence meets
    /// or exceeds `preferred`, returns the *largest* declared
    /// cadence as a best effort. Returns `None` only when the tree
    /// has no declared cadences at all (which
    /// [`CadenceTree::plan`] disallows).
    ///
    /// Per SRD-42 §"SQLite — near-time persistence": "If `10s` is
    /// in the declared cadence list, use it. Otherwise, use the
    /// next-higher declared cadence above `10s`." This helper
    /// implements that general rule for any preferred interval.
    pub fn align_to_declared(&self, preferred: Duration) -> Option<Duration> {
        let mut declared_sorted: Vec<Duration> = self.declared.iter().collect();
        declared_sorted.sort_unstable();
        if declared_sorted.is_empty() {
            return None;
        }
        declared_sorted
            .iter()
            .copied()
            .find(|&d| d >= preferred)
            .or_else(|| declared_sorted.last().copied())
    }
}

/// Walk adjacent pairs in `layers` (sorted ascending). For each gap
/// where `b/a > k`, insert geometrically-spaced intermediates rounded
/// to a human-friendly duration. Returns the inserted intermediates,
/// in insertion order, paired with the (a, b) bracket they filled.
fn synthesize_intermediates(
    layers: &mut Vec<Duration>,
    k: u32,
) -> Vec<(Duration, Duration, Duration)> {
    let mut inserted: Vec<(Duration, Duration, Duration)> = Vec::new();
    if k < 2 || layers.len() < 2 {
        return inserted;
    }
    let k_f = k as f64;

    let max_rounds = 8;
    for _ in 0..max_rounds {
        let mut changed = false;
        let mut i = 0;
        while i + 1 < layers.len() {
            let a = layers[i];
            let b = layers[i + 1];
            let ratio = b.as_secs_f64() / a.as_secs_f64().max(f64::EPSILON);
            if ratio <= k_f {
                i += 1;
                continue;
            }
            let n_steps = (ratio.ln() / k_f.ln()).ceil().max(1.0) as u32;
            let n_inserts = n_steps.saturating_sub(1).max(1);
            let step_ratio = ratio.powf(1.0 / (n_inserts as f64 + 1.0));

            let mut new_intervals: Vec<Duration> = Vec::with_capacity(n_inserts as usize);
            for j in 1..=n_inserts {
                let raw_secs = a.as_secs_f64() * step_ratio.powi(j as i32);
                let nice = nicest_duration(raw_secs);
                if nice > a
                    && nice < b
                    && !new_intervals.contains(&nice)
                    && !layers[..=i].contains(&nice)
                    && !layers[i + 1..].contains(&nice)
                {
                    new_intervals.push(nice);
                    inserted.push((a, b, nice));
                }
            }

            if new_intervals.is_empty() {
                let mid_secs = (a.as_secs_f64() * step_ratio).round().max(1.0) as u64;
                let mid = Duration::from_secs(mid_secs);
                if mid > a && mid < b {
                    new_intervals.push(mid);
                    inserted.push((a, b, mid));
                }
            }

            if !new_intervals.is_empty() {
                let insert_at = i + 1;
                for (off, d) in new_intervals.iter().enumerate() {
                    layers.insert(insert_at + off, *d);
                }
                changed = true;
                continue;
            }
            i += 1;
        }
        if !changed {
            break;
        }
    }

    layers.sort_unstable();
    layers.dedup();
    inserted
}

/// "Nice" durations the planner prefers when rounding intermediate
/// layers, ascending. Picked to be readable at a glance and consistent
/// with operator instinct (5s, 10s, 30s, 1m, …).
const NICE_SECONDS: &[u64] = &[
    1,
    2,
    5,
    10,
    15,
    20,
    30,
    45,
    60,
    2 * 60,
    5 * 60,
    10 * 60,
    15 * 60,
    20 * 60,
    30 * 60,
    45 * 60,
    3600,
    2 * 3600,
    3 * 3600,
    4 * 3600,
    6 * 3600,
    8 * 3600,
    12 * 3600,
    24 * 3600,
    2 * 86_400,
    7 * 86_400,
];

/// Round a duration (given in seconds, possibly fractional) to the
/// nearest entry in [`NICE_SECONDS`] by log-ratio — preserves the
/// geometric center of the bracket better than linear rounding.
fn nicest_duration(secs: f64) -> Duration {
    if !secs.is_finite() || secs <= 0.0 {
        return Duration::from_secs(1);
    }
    let target = secs.ln();
    let best = NICE_SECONDS
        .iter()
        .min_by(|a, b| {
            let da = ((**a as f64).ln() - target).abs();
            let db = ((**b as f64).ln() - target).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .copied()
        .unwrap_or_else(|| secs.round() as u64);
    Duration::from_secs(best)
}

/// Render a duration as a human-friendly string (`10s`, `1m`, `1h`,
/// `1h30m`). Operators reading the realized-tree log line should be
/// able to mentally compare these to the cadences they declared.
pub fn format_duration_short(d: Duration) -> String {
    let total = d.as_secs();
    if total == 0 {
        // Sub-second cadences (e.g. a 100ms optimizer finest layer)
        // render in milliseconds rather than collapsing to "0s".
        let ms = d.subsec_millis();
        return if ms == 0 {
            "0s".into()
        } else {
            format!("{ms}ms")
        };
    }
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    match (h, m, s) {
        (h, 0, 0) if h > 0 => format!("{h}h"),
        (h, m, 0) if h > 0 => format!("{h}h{m}m"),
        (0, m, 0) if m > 0 => format!("{m}m"),
        (0, 0, s) => format!("{s}s"),
        (0, m, s) if m > 0 => format!("{m}m{s}s"),
        (h, m, s) => format!("{h}h{m}m{s}s"),
    }
}

fn log_realized_tree(realized: &[CadenceLayer], max_fan_in: u32) {
    // One-line summary of the realized cadence layout:
    //   `metrics: cadences: [1s, (10s), 30s, 1m, 5m] / 20`
    // Parens mark auto-inserted layers — synthesized by the
    // planner to keep adjacent fan-in within `max_fan_in:1`
    // when the declared layers are too far apart. The
    // trailing `/ <n>` reads as "max fan-in 20:1".
    let cadences_str = realized
        .iter()
        .map(|l| {
            let s = format_duration_short(l.interval);
            if l.hidden { format!("({s})") } else { s }
        })
        .collect::<Vec<_>>()
        .join(", ");
    crate::diag::info(&format!(
        "metrics: cadences: [{cadences_str}] / {max_fan_in}"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_parse_normal() {
        let c = Cadences::parse("10s,1m,10m,1h").unwrap();
        let got: Vec<_> = c.iter().collect();
        assert_eq!(
            got,
            vec![
                Duration::from_secs(10),
                Duration::from_secs(60),
                Duration::from_secs(600),
                Duration::from_secs(3600),
            ]
        );
    }

    #[test]
    fn cadence_parse_whitespace_and_units() {
        let c = Cadences::parse(" 30s , 5m,2h ").unwrap();
        let got: Vec<_> = c.iter().collect();
        assert_eq!(got[0], Duration::from_secs(30));
        assert_eq!(got[1], Duration::from_secs(300));
        assert_eq!(got[2], Duration::from_secs(7200));
    }

    #[test]
    fn cadence_parse_accepts_sub_second() {
        // Sub-second values are now accepted at parse time — base-
        // interval validity is checked at plan time by
        // [`CadenceTree::plan_validated`].
        let c = Cadences::parse("500ms,1s").unwrap();
        let got: Vec<_> = c.iter().collect();
        assert_eq!(got[0], Duration::from_millis(500));
        assert_eq!(got[1], Duration::from_secs(1));
    }

    #[test]
    fn cadence_tree_plan_validated_rejects_below_base() {
        let c = Cadences::new(&[Duration::from_millis(500), Duration::from_secs(1)]).unwrap();
        let err =
            CadenceTree::plan_validated(c, DEFAULT_MAX_FAN_IN, Duration::from_secs(1)).unwrap_err();
        assert!(matches!(err, CadenceTreeError::BelowBase { .. }));
    }

    #[test]
    fn cadence_tree_plan_validated_rejects_non_multiple() {
        let c = Cadences::new(&[Duration::from_millis(1500)]).unwrap();
        let err =
            CadenceTree::plan_validated(c, DEFAULT_MAX_FAN_IN, Duration::from_secs(1)).unwrap_err();
        assert!(matches!(err, CadenceTreeError::NotMultiple { .. }));
    }

    #[test]
    fn align_to_declared_picks_smallest_above_preferred() {
        let c = Cadences::parse("5s,10s,1m,5m").unwrap();
        let tree = CadenceTree::plan(c, DEFAULT_MAX_FAN_IN);
        assert_eq!(
            tree.align_to_declared(Duration::from_secs(1)),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            tree.align_to_declared(Duration::from_secs(10)),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            tree.align_to_declared(Duration::from_secs(30)),
            Some(Duration::from_secs(60))
        );
        // Preferred > largest → falls back to largest.
        assert_eq!(
            tree.align_to_declared(Duration::from_secs(3600)),
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn cadence_tree_plan_validated_accepts_exact_multiple() {
        let c = Cadences::new(&[Duration::from_secs(1), Duration::from_secs(10)]).unwrap();
        let tree =
            CadenceTree::plan_validated(c, DEFAULT_MAX_FAN_IN, Duration::from_secs(1)).unwrap();
        assert_eq!(tree.layers().len(), 2);
    }

    #[test]
    fn cadence_tree_inserts_hidden_for_large_ratio() {
        let tree = CadenceTree::plan(
            Cadences::parse("10s,1m,10m,10h").unwrap(),
            DEFAULT_MAX_FAN_IN,
        );
        let layers: Vec<Duration> = tree.layers().iter().map(|l| l.interval).collect();
        for d in [10, 60, 600, 36000].iter().map(|s| Duration::from_secs(*s)) {
            assert!(layers.contains(&d));
        }
        assert!(tree.hidden().count() >= 1);
    }
}
