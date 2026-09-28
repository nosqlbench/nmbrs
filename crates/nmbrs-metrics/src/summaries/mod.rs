// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! **Summaries** — retained, transforming views of instrument data.
//!
//! A summary sits *downstream* of a primary instrument (see
//! [`crate::instruments`]) or the scheduler's cascade output,
//! and holds a transformed view of that data for on-demand
//! reads. Unlike primary instruments — recorded into on the hot
//! path, drained on every scheduler tick — a summary:
//!
//! - **Is fed from outside the hot path.** Typically attached as
//!   a consumer of a rate, a gauge, a counter's rate-of-change, or
//!   a histogram snapshot. Recording into a summary is not on the
//!   workload's critical path.
//! - **Retains data across drains.** A primary histogram resets on
//!   `capture_delta`; a summary keeps whatever representation it's
//!   built up, up to its caller-specified bound.
//! - **Transforms the stream.** Binomial reduction, sliding-window
//!   aggregation, or lossless sorted accumulation — the summary
//!   decides how to compact the stream.
//! - **Is read on demand.** Snapshots are non-mutating clones; a
//!   display can poll arbitrarily without perturbing state.
//! - **Attaches for a scope.** The lifetime is usually a UI view
//!   (a TUI panel, a report cell) or a phase. When the source
//!   stops feeding it, the summary's contents are naturally
//!   frozen — no further records arrive, so the last snapshot
//!   remains the durable artifact.
//!
//! ## What lives here
//!
//! - [`binomial_summary::BinomialSummary`] — bounded sparkline
//!   buffer with binomial (pairwise-averaging) reduction on
//!   overflow. Used for phase-scoped throughput sparklines.
//! - [`ewma::Ewma`] — time-weighted exponentially-weighted
//!   moving average. Single-scalar "current value" readout
//!   that doesn't flicker when the raw stream is noisy.
//! - [`f64stats::F64Stats`] — lossless f64 accumulator with
//!   exact percentiles. Used for relevancy scores (recall,
//!   precision).
//! - [`hdr_summary::HdrSummary`] — retained, non-draining HDR
//!   histogram for lossless latency percentiles over a scope
//!   (whole phase, whole run, analysis window).
//! - [`live_window::LiveWindowHistogram`] — sliding-window HDR
//!   histogram with lazy per-slot reset. Optional live-peek
//!   sibling of [`crate::instruments::timer::Timer`] for the
//!   TUI's 1-second rolling latency view.
//! - [`peak_tracker::PeakTracker`] — rolling max (or min) over
//!   a wall-clock window via a monotonic deque. O(1) amortized.
//!
//! See SRD 62 §"Design notes → Per-phase sparkline: binomial-
//! reduction summary instrument" for the architectural position.

pub mod binomial_summary;
pub mod ewma;
pub mod f64stats;
pub mod hdr_summary;
pub mod live_window;
pub mod peak_tracker;
