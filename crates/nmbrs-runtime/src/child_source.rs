// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-92 / ExecUnification — Step 5a: the unified child-stream contract.
//!
//! One interface by which a unit exposes its children to the executor:
//! [`ChildSource`] (`poll_next` + `realizability`). The executor STEERS on the
//! realizability level (the 03b companion) to pick a drive — bounded-spawn for
//! small/realized child sets, cursor-reserve for huge/ranged ones — while the
//! two drive primitives stay distinct (SRD-02 "One Walker = one configured-limit
//! shape, not one literal harness").
//!
//! This is the ADDITIVE foundation (Step 5a): the trait + enums, with no
//! callers yet. Step 5b implements it over the as-built sources (the scenario
//! node slice, the comprehension generator, the polydat `DataSource` cursor);
//! 5c routes by level; 5d/5e migrate the aggregate drives and the per-cycle
//! stream onto it. The op-curry stays FROZEN (not migrated) — see
//! `local/ExecUnification/11_step5_plan.md`.

// WIP SRD-92 Step 5a — the contract has no non-test callers yet; the allow is
// removed when 5b/5d wire the as-built sources and drives onto it.
#![allow(dead_code)]

use std::ops::Range;

use polydat::iteration::source::DataSource;

/// How knowable a [`ChildSource`]'s child set is — a graded PROPERTY the
/// executor reads to steer (03b). Cumulative ladder (higher subsumes lower):
/// - `Dynamic`   — pull-only; count unknown ahead (poll / open-ended).
/// - `Countable` — total count known ([`ChildSource::extent`]), not ordinal-indexable.
/// - `Rangeable` — ordinal-indexable; a pull may reserve a whole `Range` (the
///   cursor / cycle stream — `poll_next` IS `DataSource::reserve(stride)`).
/// - `Realizable` — fully materializable ahead (pre-map / dryrun / plan).
///
/// Declared in ascending order so the executor can compare with `>=` when
/// steering. NOT the same as `WrapperLevel` (which is *where a layer is legal*).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Realizability {
    Dynamic,
    Countable,
    Rangeable,
    Realizable,
}

/// One child yielded by [`ChildSource::poll_next`]. Two admissible shapes (the
/// `10` contract) — the AGGREGATE side only; the frozen op-curry is NOT a
/// `ChildSource`:
/// - `Node(index)` — a distinct sub-unit at `index` in the parent's child list
///   (scenario node / comprehension instance); the walker drives `nodes[index]`,
///   resolving its scope by position (as today). The `Realizable`/`Countable`
///   shape: small, distinct sub-units.
/// - `Ordinals(range)` — a reserved ordinal range over the SHARED body (the
///   op-chain built once); the cursor drive runs it per ordinal. The
///   `Rangeable`/`Dynamic` shape: how the 1M cycle stream avoids materializing
///   N units (a pull yields a range, not a unit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Child {
    Node(usize),
    Ordinals(Range<u64>),
}

/// SRD-92 / ExecUnification — the one interface by which a unit exposes its
/// children. Lazy: `poll_next` produces children on demand and may yield a whole
/// range, so nothing is materialized ahead of need. Re-invocable — the LOOP owns
/// re-driving (Step 6); the source does not know it is being looped. Stable
/// SOURCE order is part of the contract (cursor ordinal order; declaration order
/// for realized children) — execution/completion order may differ under
/// concurrency, which the two-latch fold (Step 3) tolerates.
///
/// Object-safe (no generics, no `Self`-by-value) so the executor can hold a
/// `&mut dyn ChildSource`.
pub trait ChildSource {
    /// Pull the next child, or `None` when drained.
    fn poll_next(&mut self) -> Option<Child>;

    /// The steering property (see [`Realizability`]).
    fn realizability(&self) -> Realizability;

    /// Total child count when known (`Countable` and up) — queried for planning
    /// (pre-map / progress / partition) WITHOUT draining the stream. `None` for
    /// `Dynamic`. Best-effort for extending streams (a re-readable snapshot).
    fn extent(&self) -> Option<u64> {
        None
    }
}

/// SRD-92 / ExecUnification Step 5b — `ChildSource` over a realized,
/// distinct-sub-unit list (the scenario node slice / comprehension instances):
/// yields `Node(0..len)` in declaration order. `Realizable` — the full set is
/// known ahead (pre-map / dryrun / plan). The executor (5d) resolves `Node(i)`
/// to `nodes[i]` + its positional scope (as today).
pub struct CountedSource {
    next: usize,
    len: usize,
}

impl CountedSource {
    pub fn new(len: usize) -> Self {
        Self { next: 0, len }
    }
}

impl ChildSource for CountedSource {
    fn poll_next(&mut self) -> Option<Child> {
        if self.next >= self.len {
            return None;
        }
        let i = self.next;
        self.next += 1;
        Some(Child::Node(i))
    }
    fn realizability(&self) -> Realizability {
        Realizability::Realizable
    }
    fn extent(&self) -> Option<u64> {
        Some(self.len as u64)
    }
}

/// SRD-92 / ExecUnification Step 5b — `ChildSource` over a polydat `DataSource`
/// cursor (the per-cycle / dataset stream). `poll_next` IS `reserve(stride)`,
/// yielding an ordinal `Range` over the SHARED body — so it never materializes N
/// units. `Rangeable` when the source has a known `extent` (bounded → plannable),
/// else `Dynamic` (unbounded → pull-only for planning). Holds a boxed source,
/// matching the as-built `DataSourceFactory::create_reader()` output (5e wires
/// the `FiberPool` onto it).
pub struct CursorSource {
    source: Box<dyn DataSource>,
    stride: usize,
}

impl CursorSource {
    pub fn new(source: Box<dyn DataSource>, stride: usize) -> Self {
        Self { source, stride }
    }

    /// Render a previously-reserved ordinal to its source item — the per-cycle,
    /// fiber-local fetch (delegates to the inner `DataSource::render_item`).
    /// Separate from the drive (`poll_next` = `reserve`); the hot loop calls
    /// this once per ordinal — `#[inline]` so it costs exactly the inner
    /// `render_item` (zero wrapper overhead on the per-cycle path).
    #[inline]
    pub fn render(&self, ordinal: u64) -> polydat::iteration::source::SourceItem {
        self.source.render_item(ordinal)
    }
}

impl ChildSource for CursorSource {
    fn poll_next(&mut self) -> Option<Child> {
        self.source.reserve(self.stride).map(Child::Ordinals)
    }
    fn realizability(&self) -> Realizability {
        if self.source.extent().is_some() {
            Realizability::Rangeable
        } else {
            Realizability::Dynamic
        }
    }
    fn extent(&self) -> Option<u64> {
        self.source.extent()
    }
}

/// SRD-22 cover-once — distributes ONE reserved ordinal `Range` (the
/// output of [`CursorSource::poll_next`]) across a stanza's ops. Each
/// yielded `(pos, base, run_len)` says: the op at stanza position `pos`
/// covers the contiguous sub-run `[base, base + run_len)`.
///
/// `run_len == per_pos_rows[pos]` (the op's `rows_per_op`) for every op
/// EXCEPT the last one at the cursor tail: `reserve` truncates the final
/// reservation to `[P, min(P + stride, end))`, so the last op receives
/// the short remainder. Because `Σ per_pos_rows == the reserved stride`,
/// the sub-runs tile the reserved range end-to-end with no gap and no
/// overlap, and consecutive stanzas reserve disjoint ranges — so every
/// ordinal in the phase's cursor space is covered EXACTLY ONCE, the
/// partial tail included (never over-read, never dropped).
///
/// Zero-alloc (a running cursor over the slice), so it drops straight
/// into the per-stanza hot loop; it is also the single, directly
/// unit-testable expression of the distribution the executor performs.
pub struct StanzaRuns<'a> {
    per_pos_rows: &'a [usize],
    pos: usize,
    base: u64,
    end: u64,
}

impl<'a> StanzaRuns<'a> {
    pub fn new(range: Range<u64>, per_pos_rows: &'a [usize]) -> Self {
        Self {
            per_pos_rows,
            pos: 0,
            base: range.start,
            end: range.end,
        }
    }
}

impl<'a> Iterator for StanzaRuns<'a> {
    /// `(stanza position, sub-run base ordinal, sub-run length)`.
    type Item = (usize, u64, usize);

    fn next(&mut self) -> Option<Self::Item> {
        // Stop at the end of the stanza OR when the reserved run is
        // exhausted mid-stanza (a short tail truncated the reservation).
        if self.pos >= self.per_pos_rows.len() || self.base >= self.end {
            return None;
        }
        // `.max(1)` mirrors the executor's guard: an op never consumes
        // zero ordinals, so the cursor always makes progress.
        let rows = self.per_pos_rows[self.pos].max(1) as u64;
        let sub_end = (self.base + rows).min(self.end);
        let item = (self.pos, self.base, (sub_end - self.base) as usize);
        self.pos += 1;
        self.base = sub_end;
        Some(item)
    }
}

/// Which drive primitive the executor uses for a child set. The two stay
/// distinct (SRD-02 "One Walker = one configured-limit shape, not one literal
/// harness"); this is the single point that chooses between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drive {
    /// Bounded-spawn `JoinSet` (Semaphore-gated; `Bounded(1)` = serial) — small,
    /// realized, distinct sub-units (scenario / comprehension / loop body).
    BoundedSpawn,
    /// `FiberPool` cursor-reserve — huge / ranged ordinal streams (the cycle
    /// stream); can't spawn 1M `JoinSet` tasks.
    CursorReserve,
}

/// SRD-92 / ExecUnification Step 5c — the level→drive selector (03b steering),
/// in one place. `Realizable`/`Countable` sources yield distinct sub-units
/// (`Child::Node`) → bounded-spawn; `Rangeable`/`Dynamic` sources yield ordinal
/// ranges (`Child::Ordinals`) → cursor-reserve. The level and the child KIND
/// partition identically (Node sources are Realizable/Countable; Ordinals
/// sources are Rangeable/Dynamic), so steering on the level *is* steering on the
/// kind. The loop (how-many-passes) is a SEPARATE axis (Step 6), above the source.
pub fn select_drive(realizability: Realizability) -> Drive {
    match realizability {
        Realizability::Realizable | Realizability::Countable => Drive::BoundedSpawn,
        Realizability::Rangeable | Realizability::Dynamic => Drive::CursorReserve,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polydat::iteration::source::{DataSourceFactory, RangeSourceFactory};

    #[test]
    fn counted_source_is_realizable_node_indices() {
        let mut s = CountedSource::new(3);
        assert_eq!(s.realizability(), Realizability::Realizable);
        assert_eq!(s.extent(), Some(3));
        let mut got = vec![];
        while let Some(Child::Node(i)) = s.poll_next() {
            got.push(i);
        }
        assert_eq!(got, vec![0, 1, 2]);
    }

    #[test]
    fn cursor_source_over_range_is_rangeable_and_yields_ordinals() {
        // poll_next IS reserve(stride): stride-2 over [0,5) → 0..2, 2..4, 4..5.
        let f = RangeSourceFactory::new(0, 5);
        let mut s = CursorSource::new(f.create_reader(), 2);
        assert_eq!(s.realizability(), Realizability::Rangeable);
        assert_eq!(s.extent(), Some(5));
        let mut ranges = vec![];
        while let Some(Child::Ordinals(r)) = s.poll_next() {
            ranges.push(r);
        }
        assert_eq!(ranges, vec![0..2, 2..4, 4..5]);
    }

    #[test]
    fn realizability_ladder_is_ordered() {
        assert!(Realizability::Dynamic < Realizability::Countable);
        assert!(Realizability::Countable < Realizability::Rangeable);
        assert!(Realizability::Rangeable < Realizability::Realizable);
    }

    #[test]
    fn select_drive_routes_by_level() {
        assert_eq!(select_drive(Realizability::Realizable), Drive::BoundedSpawn);
        assert_eq!(select_drive(Realizability::Countable), Drive::BoundedSpawn);
        assert_eq!(select_drive(Realizability::Rangeable), Drive::CursorReserve);
        assert_eq!(select_drive(Realizability::Dynamic), Drive::CursorReserve);
    }

    #[test]
    fn select_drive_aligns_with_source_kinds() {
        // a Node-yielding source drives via bounded-spawn; an Ordinals-yielding
        // (cursor) source drives via cursor-reserve.
        assert_eq!(
            select_drive(CountedSource::new(2).realizability()),
            Drive::BoundedSpawn
        );
        let cursor = CursorSource::new(RangeSourceFactory::new(0, 4).create_reader(), 2);
        assert_eq!(select_drive(cursor.realizability()), Drive::CursorReserve);
    }

    // === SRD-22 batching cover-once (StanzaRuns) ============================

    #[test]
    fn stanza_runs_cover_once_over_cursor_with_partial_tail() {
        // The real driver path: a phase cursor over `[0, M)` with a SINGLE
        // batch op of rows_per_op = N. M is NOT divisible by N, so the final
        // reservation is a short tail — the exact case the coordinator flagged.
        const M: u64 = 1000;
        const N: usize = 300;
        let per_pos_rows = [N]; // one op per stanza → Σ rows_per_op = N
        let stanza_stride = N;

        // `poll_next` IS `reserve(stanza_stride)`; the tail run is truncated.
        let mut source =
            CursorSource::new(RangeSourceFactory::new(0, M).create_reader(), stanza_stride);

        let mut covered: Vec<(u64, usize)> = Vec::new(); // (base, run_len)
        let mut next_expected: u64 = 0;
        while let Some(Child::Ordinals(range)) = source.poll_next() {
            for (pos, base, run_len) in StanzaRuns::new(range, &per_pos_rows) {
                assert_eq!(pos, 0, "single-op stanza is always position 0");
                // NO skip / NO overlap: each sub-run starts exactly where the
                // previous one ended.
                assert_eq!(base, next_expected, "gap or overlap at ordinal {base}");
                next_expected = base + run_len as u64;
                covered.push((base, run_len));
            }
        }

        // Exact tiling: [0,300) [300,600) [600,900) [900,1000).
        assert_eq!(covered, vec![(0, 300), (300, 300), (600, 300), (900, 100)]);
        // The final op consumes the M % N = 100-row remainder — the partial
        // tail is inserted, not dropped.
        assert_eq!(covered.last().unwrap().1, (M % N as u64) as usize);
        // Total ordinals consumed == M exactly (NOT M×N — the over-insert bug
        // this fix removes).
        let total: usize = covered.iter().map(|&(_, len)| len).sum();
        assert_eq!(total as u64, M);
    }

    #[test]
    fn stanza_runs_multi_op_stanza_tiles_reserved_stride() {
        // Stanza of two ops: op0 rows_per_op=1 (ordinary), op1 rows_per_op=4
        // (batch). stride = 5. A FULL reservation `[10, 15)` splits into
        // op0 → [10,11) and op1 → [11,15).
        let per_pos_rows = [1, 4];
        let runs: Vec<_> = StanzaRuns::new(10..15, &per_pos_rows).collect();
        assert_eq!(runs, vec![(0, 10, 1), (1, 11, 4)]);
        // Σ run_len == the reserved stride; the last run ends exactly at
        // range.end (disjoint from the next stanza's reservation).
        assert_eq!(runs.iter().map(|&(_, _, l)| l).sum::<usize>(), 5);
    }

    #[test]
    fn stanza_runs_truncates_short_tail_mid_stanza() {
        // Reserved range shorter than the stanza stride (a tail). Stanza
        // `[1, 4]` (stride 5) over `[12, 15)`: op0 → [12,13); op1 nominal 4
        // but only 2 remain → [13,15) (run_len 2); no over-read past 15.
        let per_pos_rows = [1, 4];
        let runs: Vec<_> = StanzaRuns::new(12..15, &per_pos_rows).collect();
        assert_eq!(runs, vec![(0, 12, 1), (1, 13, 2)]);
        assert_eq!(runs.iter().map(|&(_, _, l)| l).sum::<usize>(), 3);

        // A range that ends before the second op even starts: op1 gets
        // nothing, and the iterator stops cleanly (no zero-length run).
        let runs2: Vec<_> = StanzaRuns::new(14..15, &per_pos_rows).collect();
        assert_eq!(runs2, vec![(0, 14, 1)]);
    }
}
