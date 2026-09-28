// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Polydat-side metric-reading node registration.
//!
//! Formerly lived at `polydat::library::metrics`. Moved here so polydat
//! can publish to crates.io without a reverse dep on `nmbrs-metrics`.
//! `inventory` is the registration channel — polydat's
//! `register_nodes!` macro emits an `inventory::submit!` block from
//! this crate; polydat picks it up at link time without knowing
//! who registered it.
//!
//! Polydat node functions for reading live metrics from the unified
//! [`MetricsQuery`] (SRD-42 §"MetricsQuery").
//!
//! - `metric(label_pattern, stat)` — reads
//!   [`MetricsQuery::session_lifetime`]: session running **totals**
//!   (`cycles`/`errors`) and the session-average `rate` (total ÷ session age).
//! - `metric_window(label_pattern, stat)` — at the smallest cadence: the
//!   latest window's per-window **increase** via
//!   [`MetricsQuery::increase_over`] (`cycles`/`errors`, and `rate` =
//!   increase ÷ interval), and its latency **distribution** via
//!   [`MetricsQuery::distribution_over`] (`p50`/`p99`/`mean`).
//!
//! Both are non-deterministic context nodes. In strict mode they
//! require explicit acknowledgment. The query reference is captured
//! at node construction from a global static set by the runner.
//!
//! ## Stat accessors (PromQL-aligned word stems)
//!
//! - `"cycles"` — cycles_total: session **total** (`metric`) / per-window
//!   **increase** (`metric_window`)
//! - `"errors"` — errors_total: session total / per-window increase
//! - `"rate"` — cycles/second (session-average / per-window)
//! - `"p50"`, `"p99"`, `"mean"` — latency quantiles from cycles_servicetime (nanos)

use std::sync::{Arc, LazyLock, Mutex};

// Node metadata + registration are emitted by `#[polydat::polydat_node]`
// (fully-qualified `polydat::…` paths, including the `Const<…>` marker), so
// no `polydat::ast` / `polydat::dsl::registry` imports are needed here.
use crate::metrics_query::{MetricsQuery, Selection};
use crate::snapshot::{MetricSet, MetricValue};

/// Global metrics query reference. Set by the runner once the
/// cadence reporter is built. Polydat metric nodes capture this at
/// construction time.
static METRICS_QUERY: LazyLock<Mutex<Option<Arc<MetricsQuery>>>> =
    LazyLock::new(|| Mutex::new(None));

/// Set the global metrics query for Polydat node access.
pub fn set_global_query(query: Arc<MetricsQuery>) {
    *METRICS_QUERY.lock().unwrap_or_else(|e| e.into_inner()) = Some(query);
}

/// Get the global metrics query reference.
fn get_query() -> Option<Arc<MetricsQuery>> {
    METRICS_QUERY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Build a [`Selection`] from a `"family, key=value, key~substring"`
/// pattern, returning the family name (when one was given) alongside.
/// A bare token — no `=` / `~` — names the metric FAMILY, i.e. the
/// registered instrument name (`result_failure`, `attempt_failure`,
/// `cycles_servicetime`, …): the same names metrics.db carries, so
/// the reader vocabulary and the metric namespace are one. Labeled
/// parts narrow to matching series within the selection.
fn selection_from_pattern(pattern: &str) -> (Selection, Option<String>) {
    let mut sel = Selection::all();
    let mut family: Option<String> = None;
    for part in pattern.split(',').map(str::trim) {
        if part.is_empty() {
            continue;
        }
        if let Some((key, value)) = part.split_once('=') {
            sel = sel.with_label(key.trim(), value.trim());
        } else if let Some((key, substring)) = part.split_once('~') {
            sel = sel.with_label_containing(key.trim(), substring.trim());
        } else {
            sel = sel.with_family(part);
            family = Some(part.to_string());
        }
    }
    (sel, family)
}

/// Read a stat from the canonical session-lifetime view.
///
/// Signature: `metric(label_pattern: const str, stat: const str) -> f64`.
/// Reads [`MetricsQuery::session_lifetime`] — session running totals
/// (`cycles`/`errors`) and the session-average `rate`. Authored via
/// `#[polydat::polydat_node]` (SRD-80b).
///
/// Intrinsically `Nondeterministic`: reads the live session-lifetime view
/// the cadence pipeline frames, so the node is never const-folded and is
/// re-evaluated on every pull — a metrics reader can never cache a stale
/// (e.g. compile-time-empty) value.
#[polydat::polydat_node(
    category = Context,
    purity = Nondeterministic("reads live session-lifetime metrics; value changes over the run"),
)]
fn metric(label_pattern: Const<&str>, stat: Const<&str>) -> f64 {
    let (sel, family) = selection_from_pattern(label_pattern.0);
    let Some(fam) = family else {
        warn_family_required("metric", label_pattern.0);
        return 0.0;
    };
    get_query()
        .map(|q| q.session_lifetime(&sel))
        .and_then(|snap| extract_family_stat(&snap, &fam, stat.0))
        .unwrap_or(0.0)
}

/// Read a stat from the latest closed smallest-cadence window.
///
/// Signature: `metric_window(label_pattern: const str, stat: const str) ->
/// f64`. Counter stats read the per-window INCREASE (`cycles`/`errors`, and
/// `rate` = increase ÷ interval); latency quantiles (`p50`/`p99`/`mean`)
/// read the merged window DISTRIBUTION. Contrast [`metric`], which reads
/// session-lifetime running totals.
///
/// Intrinsically `Nondeterministic` — see [`metric`].
#[polydat::polydat_node(
    category = Context,
    purity = Nondeterministic("reads the latest framed cadence window; value changes over the run"),
)]
fn metric_window(label_pattern: Const<&str>, stat: Const<&str>) -> f64 {
    let (sel, family) = selection_from_pattern(label_pattern.0);
    let Some(family) = family else {
        warn_family_required("metric_window", label_pattern.0);
        return 0.0;
    };
    get_query()
        .and_then(|q| {
            let smallest = q.reporter().declared_cadences().smallest();
            if smallest.is_zero() {
                return None;
            }
            let snap = match stat.0 {
                "p50" | "p99" | "mean" => q.distribution_over(smallest, &sel),
                _ => q.increase_over(smallest, &sel),
            };
            extract_family_stat(&snap, &family, stat.0)
        })
        .unwrap_or(0.0)
}

/// Extract a named stat from a [`MetricSet`].
/// A family-less pattern has nothing to read since the canned stat
/// vocabulary was retired (2026-07-10): warn ONCE per distinct
/// pattern — visible, never silent, and not per-eval spam on the
/// objective/guard evaluation paths — and let the reader yield 0.0.
fn warn_family_required(node: &str, pattern: &str) {
    static WARNED: LazyLock<Mutex<std::collections::HashSet<String>>> =
        LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));
    let key = format!("{node}:{pattern}");
    let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
    if warned.insert(key) {
        crate::diag::warn(&format!(
            "{node}('{pattern}', …): no metric family named — the first \
             bare token in the pattern must be an instrument name (e.g. \
             {node}('errors_total', 'rate')); reading 0.0"
        ));
    }
}

/// Read `stat` from the NAMED family's first series — the ONE
/// vocabulary: any registered counter / gauge / histogram is
/// readable by its own name. Stats: `count` (counter cumulative or
/// histogram count), `value` (gauge, falling back to counter
/// cumulative), `rate` (counter cumulative over the snapshot's
/// interval — the session average for `metric`, the window rate for
/// `metric_window`), `mean` / `p50` / `p99` (histograms). Returns
/// `None` — surfaced as `0.0` by the reader nodes — when the family
/// is absent from the snapshot or the stat doesn't apply to its
/// type.
/// SRD-75 (C5) — does a `metric()`-style selector currently resolve to
/// at least one REGISTERED instrument? True when the pattern names a
/// family and the session-lifetime view contains ≥1 instance matching
/// the full selection. The phase-poll `require:` strict gate polls
/// this within its grace window; the `metric()` readers themselves
/// stay lenient (0.0 + one-shot warn) because a stop predicate over a
/// not-yet-registered family is a legitimate "not yet" state — only a
/// coordination GATE treats absence as a bug.
pub fn metric_selector_resolves(pattern: &str) -> bool {
    let (sel, family) = selection_from_pattern(pattern);
    let Some(fam) = family else { return false };
    get_query()
        .map(|q| q.session_lifetime(&sel))
        .and_then(|snap| {
            snap.family(&fam)
                // Mirror `extract_family_stat` exactly: an instance with a
                // readable POINT — presence without a point still reads
                // 0.0 at the `metric()` reader, so it must not count as
                // resolved.
                .map(|f| f.metrics().next().and_then(|m| m.point()).is_some())
        })
        .unwrap_or(false)
}

fn extract_family_stat(snapshot: &MetricSet, family: &str, stat: &str) -> Option<f64> {
    let f = snapshot.family(family)?;
    let m = f.metrics().next()?;
    match (m.point()?.value(), stat) {
        (MetricValue::Counter(c), "count" | "value") => Some(c.cumulative as f64),
        (MetricValue::Counter(c), "rate") => {
            let secs = snapshot.interval().as_secs_f64().max(0.001);
            Some(c.cumulative as f64 / secs)
        }
        (MetricValue::Gauge(g), "value") => Some(g.value),
        (MetricValue::Histogram(h), "count") => Some(h.count as f64),
        (MetricValue::Histogram(h), "mean") if h.count > 0 => Some(h.reservoir.mean()),
        (MetricValue::Histogram(h), "p50") if h.count > 0 => {
            Some(h.reservoir.value_at_quantile(0.50) as f64)
        }
        (MetricValue::Histogram(h), "p99") if h.count > 0 => {
            Some(h.reservoir.value_at_quantile(0.99) as f64)
        }
        _ => None,
    }
}

// `metric` / `metric_window` are authored via `#[polydat::polydat_node]`
// above — their FuncSig + builder are macro-generated and registered through
// the macro's own `inventory::submit!`, so no hand-written `signatures()` /
// `build_node()` / `register_nodes!` is needed here.
