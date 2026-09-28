// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! WebSocket metric streaming for the live dashboard.
//!
//! A [`BroadcastReporter`] implements `nmbrs_metrics::scheduler::Reporter`
//! and forwards frames into a `tokio::sync::broadcast` channel.
//! The `/ws/metrics` handler subscribes each WebSocket client to that
//! channel and sends HTML fragments for htmx out-of-band swap.

use std::collections::VecDeque;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use tokio::sync::broadcast;

use nmbrs_metrics::scheduler::Reporter;
use nmbrs_metrics::snapshot::{MetricSet, MetricValue};

/// Number of rate samples to keep for the sparkline.
const SPARKLINE_HISTORY: usize = 60;

/// Shared state: the broadcast sender for metric frames.
///
/// Cloned into Axum state. Each WebSocket client subscribes via
/// `sender.subscribe()`.
#[derive(Clone)]
pub struct MetricsBroadcast {
    sender: broadcast::Sender<MetricSet>,
}

impl MetricsBroadcast {
    /// Create a new broadcast with the given channel capacity.
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Get a reporter that feeds frames into this broadcast.
    pub fn reporter(&self) -> BroadcastReporter {
        BroadcastReporter {
            sender: self.sender.clone(),
        }
    }

    /// Publish a frame directly to all WebSocket subscribers.
    ///
    /// Used by the HTTP ingestion endpoint to inject externally
    /// pushed metrics into the broadcast channel.
    pub fn publish(&self, snapshot: MetricSet) {
        let _ = self.sender.send(snapshot);
    }
}

/// A `Reporter` that pushes frames into a broadcast channel.
///
/// Register this with the metrics scheduler via
/// `SchedulerBuilder::add_reporter()`.
pub struct BroadcastReporter {
    sender: broadcast::Sender<MetricSet>,
}

impl Reporter for BroadcastReporter {
    fn report(&mut self, snapshot: &MetricSet) {
        // Ignore send errors — means no subscribers are connected.
        let _ = self.sender.send(snapshot.clone());
    }
}

/// WebSocket upgrade handler for `/ws/metrics`.
pub async fn metrics_ws(
    ws: WebSocketUpgrade,
    State(broadcast): State<MetricsBroadcast>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, broadcast))
}

async fn handle_ws(mut socket: WebSocket, broadcast: MetricsBroadcast) {
    let mut rx = broadcast.sender.subscribe();
    let mut rate_history: VecDeque<f64> = VecDeque::with_capacity(SPARKLINE_HISTORY);
    let mut log_row_count: u64 = 0;
    let mut prev_ops: u64 = 0;
    let mut prev_errors: u64 = 0;

    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(snapshot) => {
                        let raw = extract_raw_counters(&snapshot);
                        let interval_secs = snapshot.interval().as_secs_f64().max(0.001);

                        // Compute deltas from cumulative counters.
                        let delta_ops = raw.cumulative_ops.saturating_sub(prev_ops);
                        let delta_errors = raw.cumulative_errors.saturating_sub(prev_errors);
                        let ops_per_sec = if prev_ops > 0 {
                            delta_ops as f64 / interval_secs
                        } else {
                            // First frame — use cumulative / interval as
                            // best guess, but it will be high.
                            raw.cumulative_ops as f64 / interval_secs
                        };

                        prev_ops = raw.cumulative_ops;
                        prev_errors = raw.cumulative_errors;

                        // Track rate history for sparkline.
                        rate_history.push_back(ops_per_sec);
                        if rate_history.len() > SPARKLINE_HISTORY {
                            rate_history.pop_front();
                        }

                        log_row_count += 1;

                        let snap = DisplaySnapshot {
                            cumulative_ops: raw.cumulative_ops,
                            cumulative_errors: raw.cumulative_errors,
                            ops_per_sec,
                            delta_errors,
                            p99_display: raw.p99_display,
                        };

                        let html = render_oob_html(
                            &snap,
                            &rate_history,
                            log_row_count,
                        );
                        if socket.send(Message::Text(html.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("nmbrs web: ws client lagged, dropped {n} frames");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }
}

// ─── Raw Counter Extraction ─────────────────────────────────

struct RawCounters {
    cumulative_ops: u64,
    cumulative_errors: u64,
    p99_display: String,
}

/// Extract raw cumulative counters and P99 from a snapshot.
fn extract_raw_counters(snapshot: &MetricSet) -> RawCounters {
    let mut cumulative_ops: u64 = 0;
    let mut cumulative_errors: u64 = 0;
    let mut p99_ns: Option<u64> = None;

    for family in snapshot.families() {
        let fname = family.name();
        for metric in family.metrics() {
            let Some(point) = metric.point() else {
                continue;
            };
            match point.value() {
                MetricValue::Counter(c) => {
                    if fname.contains("ops") || fname.contains("cycles") {
                        cumulative_ops += c.cumulative;
                    }
                    if fname.contains("error") {
                        cumulative_errors += c.cumulative;
                    }
                }
                MetricValue::Histogram(h) => {
                    let p99 = h.reservoir.value_at_quantile(0.99);
                    p99_ns = Some(p99_ns.map_or(p99, |prev: u64| prev.max(p99)));
                }
                _ => {}
            }
        }
    }

    let p99_display = match p99_ns {
        Some(ns) => format!("{:.1}", ns as f64 / 1_000_000.0),
        None => "\u{2014}".into(),
    };

    RawCounters {
        cumulative_ops,
        cumulative_errors,
        p99_display,
    }
}

// ─── Display Snapshot ───────────────────────────────────────

struct DisplaySnapshot {
    cumulative_ops: u64,
    cumulative_errors: u64,
    ops_per_sec: f64,
    delta_errors: u64,
    p99_display: String,
}

// ─── OOB HTML Rendering ─────────────────────────────────────

fn render_oob_html(
    snap: &DisplaySnapshot,
    rate_history: &VecDeque<f64>,
    log_row_id: u64,
) -> String {
    let ops_display = if snap.ops_per_sec > 0.0 {
        format!("{:.0}", snap.ops_per_sec)
    } else {
        "\u{2014}".into()
    };

    let sparkline = render_sparkline_svg(rate_history);
    let log_row = render_log_row(snap, log_row_id);

    format!(
        "<div id=\"total-cycles\" hx-swap-oob=\"true\">{}</div>\n\
         <div id=\"ops-per-sec\" hx-swap-oob=\"true\">{ops_display}</div>\n\
         <div id=\"p99-latency\" hx-swap-oob=\"true\">{}</div>\n\
         <div id=\"error-count\" hx-swap-oob=\"true\">{}</div>\n\
         <div id=\"rate-sparkline\" hx-swap-oob=\"true\" style=\"overflow: hidden; height: 80px;\">{sparkline}</div>\n\
         {log_row}",
        snap.cumulative_ops, snap.p99_display, snap.cumulative_errors
    )
}

// ─── Sparkline SVG ──────────────────────────────────────────

fn render_sparkline_svg(history: &VecDeque<f64>) -> String {
    if history.is_empty() {
        return "<p style=\"color: var(--text-dim);\">Waiting for metrics...</p>".into();
    }

    let width = 800u32;
    let height = 70u32;
    let n = history.len();
    let max_rate = history.iter().cloned().fold(1.0_f64, f64::max);
    let bar_width = (width as f64 / SPARKLINE_HISTORY as f64).floor().max(2.0);
    let gap = 1.0;

    let mut bars = String::new();
    for (i, &rate) in history.iter().enumerate() {
        let bar_height = if max_rate > 0.0 {
            (rate / max_rate * height as f64).max(1.0)
        } else {
            1.0
        };
        // Right-align: newest bar at right edge.
        let x = (SPARKLINE_HISTORY - n + i) as f64 * bar_width;
        let y = height as f64 - bar_height;
        bars.push_str(&format!(
            "<rect x=\"{x:.0}\" y=\"{y:.0}\" width=\"{:.0}\" height=\"{bar_height:.0}\" \
             fill=\"var(--green)\" opacity=\"0.8\"/>",
            bar_width - gap
        ));
    }

    // Label: current rate at top-right.
    let current = history.back().unwrap_or(&0.0);
    let label = format!("{current:.0} ops/s");

    format!(
        "<svg viewBox=\"0 0 {width} {height}\" preserveAspectRatio=\"none\" \
         style=\"width: 100%; height: 100%;\">\
         {bars}\
         <text x=\"{0}\" y=\"12\" fill=\"var(--text-dim)\" font-size=\"11\" \
         font-family=\"var(--mono)\" text-anchor=\"end\">{label}</text>\
         </svg>",
        width - 4
    )
}

// ─── Log Row ────────────────────────────────────────────────

fn render_log_row(snap: &DisplaySnapshot, row_id: u64) -> String {
    let ops_display = if snap.ops_per_sec > 0.0 {
        format!("{:.0}", snap.ops_per_sec)
    } else {
        "\u{2014}".into()
    };

    format!(
        "<tbody id=\"metrics-log\" hx-swap-oob=\"afterbegin\">\
         <tr>\
         <td style=\"color: var(--text-dim);\">{row_id}</td>\
         <td style=\"color: var(--green); font-weight: 600;\">{ops_display}</td>\
         <td>{}</td>\
         <td>{}</td>\
         <td>{}</td>\
         </tr>\
         </tbody>",
        snap.p99_display, snap.cumulative_ops, snap.delta_errors
    )
}
