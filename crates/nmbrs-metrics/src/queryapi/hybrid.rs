// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-90 §M5 — the **hybrid** metric read backend.
//!
//! A composite [`MetricAccess`] over an **ordered list of tiers**, finest/
//! freshest first (today: the in-memory cadence tier, then the per-session
//! sqlite store; extensible to a remote tail, etc.). The read model:
//!
//! 1. Walk the tiers finest-first. A tier advertises the oldest time it can
//!    answer ([`Tier::earliest_ms`]). Query a tier only while the query is **not
//!    yet covered back to `start`** by the finer tiers already chosen — and skip
//!    a tier whose data begins after the window (no intersection). Stop as soon
//!    as a tier reaches back to (or before) `start`, or advertises an unbounded
//!    horizon.
//! 2. Issue the **same** `[start_ms, end_ms]` bounds to every chosen tier,
//!    **concurrently** when more than one is needed.
//! 3. Fold the results finest-first by **union-minus-overlap**: a finer tier
//!    wins every time-span it covers, so "high-centered" recent data comes from
//!    memory at full sub-interval resolution and only the older tail is served,
//!    coarser, from the durable store. The union/overlap rule is **per series**
//!    (boundary = wherever the finer tier's own samples start), not a single
//!    global cut.
//!
//! The common case — a recent windowed read served entirely from the in-memory
//! horizon — chooses exactly one tier and never opens sqlite.

use std::collections::HashMap;
use std::sync::Arc;

use super::{Matcher, MetricAccess, QueryError, Sample, Series, Vector};

/// A horizon-advertising backend: the oldest sample-time it still holds, in
/// Unix-ms. `None` ⇒ unbounded (covers back as far as the query asks) — the
/// natural answer for a durable tail tier.
pub trait HorizonAware: Send + Sync {
    fn earliest_ms(&self) -> Option<i64>;
}

/// One tier of a [`HybridStore`]: a backend plus its horizon advertiser.
pub struct Tier {
    access: Arc<dyn MetricAccess>,
    /// Oldest answerable time (Unix-ms). `None` ⇒ unbounded tail.
    earliest_ms: Arc<dyn Fn() -> Option<i64> + Send + Sync>,
}

impl Tier {
    /// A tier whose horizon is computed on demand (e.g. the in-memory tier's
    /// retention edge).
    pub fn new(
        access: Arc<dyn MetricAccess>,
        earliest_ms: Arc<dyn Fn() -> Option<i64> + Send + Sync>,
    ) -> Self {
        Self {
            access,
            earliest_ms,
        }
    }

    /// A tier that covers back as far as any query asks (a durable tail such as
    /// sqlite) — always serves the older remainder.
    pub fn unbounded(access: Arc<dyn MetricAccess>) -> Self {
        Self {
            access,
            earliest_ms: Arc::new(|| None),
        }
    }

    fn earliest(&self) -> Option<i64> {
        (self.earliest_ms)()
    }
}

/// Composite read backend over an ordered tier list (finest first).
pub struct HybridStore {
    tiers: Vec<Tier>,
}

impl HybridStore {
    /// Compose `tiers`, finest/freshest first.
    pub fn new(tiers: Vec<Tier>) -> Self {
        Self { tiers }
    }
}

impl MetricAccess for HybridStore {
    fn select_range(
        &self,
        matchers: &[Matcher],
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vector, QueryError> {
        // 1. Coverage-aware tier selection (finest first).
        let mut chosen: Vec<&Tier> = Vec::new();
        // `covered_back_to` = oldest time covered so far (exclusive lower edge);
        // start above the window so the first intersecting tier is always taken.
        let mut covered_back_to = end_ms.saturating_add(1);
        for tier in &self.tiers {
            if covered_back_to <= start_ms {
                break; // query already covered back to `start`
            }
            let earliest = tier.earliest();
            // Skip a tier whose data begins after the window — it intersects
            // nothing in `[start, end]`.
            if let Some(e) = earliest
                && e > end_ms
            {
                continue;
            }
            chosen.push(tier);
            covered_back_to = match earliest {
                Some(e) => covered_back_to.min(e),
                None => start_ms, // unbounded tail closes the coverage
            };
        }

        // 2. Issue the same bounds to every chosen tier — concurrently when >1.
        let results: Vec<Vector> = match chosen.as_slice() {
            [] => return Ok(Vector::default()),
            [only] => vec![only.access.select_range(matchers, start_ms, end_ms)?],
            many => {
                let collected: Vec<Result<Vector, QueryError>> = std::thread::scope(|scope| {
                    let handles: Vec<_> = many
                        .iter()
                        .map(|tier| {
                            scope
                                .spawn(move || tier.access.select_range(matchers, start_ms, end_ms))
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| h.join().expect("tier query panicked"))
                        .collect()
                });
                collected.into_iter().collect::<Result<Vec<_>, _>>()?
            }
        };

        // 3. Fold finest-first; each finer accumulation wins overlaps with the
        //    next, coarser tier.
        let mut iter = results.into_iter();
        let mut acc = iter.next().unwrap_or_default();
        for coarser in iter {
            acc = union_minus_overlap(acc, coarser);
        }
        Ok(acc)
    }
}

/// Merge two range-vectors, preferring `fine` in every overlap.
///
/// Per series (matched by label *set*, order-independent): keep all of `fine`'s
/// samples, and from `coarse` keep only the samples **strictly older** than
/// `fine`'s earliest sample for that series — the non-overlapping tail. A series
/// present only in `coarse` passes through whole. Output samples are time-ordered.
fn union_minus_overlap(fine: Vector, coarse: Vector) -> Vector {
    let mut out: HashMap<Vec<(String, String)>, Series> = HashMap::new();
    for s in fine.into_series() {
        out.insert(canonical_labels(&s.labels), s);
    }
    for c in coarse.into_series() {
        let key = canonical_labels(&c.labels);
        match out.get_mut(&key) {
            Some(fine_series) => {
                // `fine`'s samples are ascending; its earliest is the overlap edge.
                let edge = fine_series.samples.first().map(|s| s.timestamp_ms);
                let mut merged: Vec<Sample> = c
                    .samples
                    .into_iter()
                    .filter(|s| edge.map_or(true, |e| s.timestamp_ms < e))
                    .collect();
                merged.append(&mut fine_series.samples);
                merged.sort_by_key(|s| s.timestamp_ms);
                fine_series.samples = merged;
            }
            None => {
                out.insert(key, c);
            }
        }
    }
    Vector::new(out.into_values().collect())
}

/// A canonical, order-independent key for a series' label set.
fn canonical_labels(labels: &[(String, String)]) -> Vec<(String, String)> {
    let mut v = labels.to_vec();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn series(name: &str, pts: &[(i64, f64)]) -> Series {
        Series {
            labels: vec![("__name__".to_string(), name.to_string())],
            samples: pts
                .iter()
                .map(|&(t, v)| Sample {
                    timestamp_ms: t,
                    value: v,
                })
                .collect(),
        }
    }

    struct Stub {
        out: Vector,
        calls: Arc<AtomicUsize>,
    }
    impl MetricAccess for Stub {
        fn select_range(&self, _: &[Matcher], _: i64, _: i64) -> Result<Vector, QueryError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.out.clone())
        }
    }

    fn tier(out: Vector, earliest: Option<i64>) -> (Tier, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let t = Tier::new(
            Arc::new(Stub {
                out,
                calls: calls.clone(),
            }),
            Arc::new(move || earliest),
        );
        (t, calls)
    }

    #[test]
    fn one_tier_covers_query_no_lower_tier_queried() {
        // mem reaches back to t=0; query [100,200] is covered by mem alone.
        let (mem, mem_calls) = tier(
            Vector::new(vec![series("ops", &[(100, 1.0), (150, 2.0), (200, 3.0)])]),
            Some(0),
        );
        let (cold, cold_calls) = tier(Vector::new(vec![series("ops", &[(100, 1.0)])]), None);
        let store = HybridStore::new(vec![mem, cold]);

        let v = store.select_range(&[], 100, 200).unwrap();
        assert_eq!(mem_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            cold_calls.load(Ordering::SeqCst),
            0,
            "cold not consulted when mem covers the query"
        );
        assert_eq!(v.series()[0].samples.len(), 3);
    }

    #[test]
    fn spill_stitches_cold_tail_under_mem_recent() {
        // mem only holds from t=150; query [0,300] needs the older tail from cold.
        let (mem, _) = tier(
            Vector::new(vec![series("ops", &[(150, 5.0), (300, 7.0)])]),
            Some(150),
        );
        let (cold, cold_calls) = tier(
            Vector::new(vec![series(
                "ops",
                &[(0, 1.0), (100, 2.0), (150, 5.0), (300, 7.0)],
            )]),
            None,
        );
        let store = HybridStore::new(vec![mem, cold]);

        let v = store.select_range(&[], 0, 300).unwrap();
        assert_eq!(
            cold_calls.load(Ordering::SeqCst),
            1,
            "cold consulted for the older tail"
        );
        let s = &v.series()[0];
        let ts: Vec<i64> = s.samples.iter().map(|x| x.timestamp_ms).collect();
        // cold's 150 & 300 (>= mem edge 150) dropped as overlap; 0 & 100 kept;
        // then mem's 150 & 300 — one smooth timeline, no double-count at 150.
        assert_eq!(ts, vec![0, 100, 150, 300]);
        assert_eq!(
            s.samples.iter().filter(|x| x.timestamp_ms == 150).count(),
            1
        );
        assert_eq!(
            s.samples
                .iter()
                .find(|x| x.timestamp_ms == 150)
                .unwrap()
                .value,
            5.0
        );
    }

    #[test]
    fn tier_whose_data_starts_after_the_window_is_skipped() {
        // Query an OLD window [0,100]; mem only holds from t=500 (after the
        // window) → mem skipped, cold serves it.
        let (mem, mem_calls) = tier(Vector::new(vec![series("ops", &[(500, 9.0)])]), Some(500));
        let (cold, cold_calls) = tier(
            Vector::new(vec![series("ops", &[(0, 1.0), (100, 2.0)])]),
            None,
        );
        let store = HybridStore::new(vec![mem, cold]);

        let v = store.select_range(&[], 0, 100).unwrap();
        assert_eq!(
            mem_calls.load(Ordering::SeqCst),
            0,
            "mem has no data in the window — skipped"
        );
        assert_eq!(cold_calls.load(Ordering::SeqCst), 1);
        assert_eq!(v.series()[0].samples.len(), 2);
    }

    #[test]
    fn series_only_in_cold_survives_the_union() {
        let (mem, _) = tier(Vector::new(vec![series("ops", &[(150, 5.0)])]), Some(150));
        let (cold, _) = tier(
            Vector::new(vec![
                series("ops", &[(150, 5.0)]),
                series("errors", &[(0, 9.0), (100, 9.0)]),
            ]),
            None,
        );
        let store = HybridStore::new(vec![mem, cold]);

        let v = store.select_range(&[], 0, 200).unwrap();
        assert_eq!(v.len(), 2);
        let errors = v
            .series()
            .iter()
            .find(|s| s.labels.iter().any(|(_, v)| v == "errors"))
            .unwrap();
        assert_eq!(errors.samples.len(), 2);
    }
}
