// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Information retrieval relevancy metrics (SRD 47).
//!
//! Pure functions for computing recall@k, precision@k, F1@k,
//! reciprocal rank, and average precision. All operate on sorted,
//! deduplicated `i64` slices representing ground truth (relevant)
//! and actual result indices.

use std::collections::HashSet;

/// Count elements present in both sorted slices.
///
/// Two-pointer O(n+m) scan — no allocation. Both slices must be
/// sorted in ascending order with no duplicates.
pub fn intersection_count(a: &[i64], b: &[i64]) -> usize {
    let (mut i, mut j, mut count) = (0, 0, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                count += 1;
                i += 1;
                j += 1;
            }
        }
    }
    count
}

/// Recall@k: fraction of the top-k ground truth items found in the results.
///
/// Both `relevant` and `actual` should be pre-truncated to at most k
/// elements and sorted. Returns `|relevant ∩ actual| / k`.
pub fn recall(relevant: &[i64], actual: &[i64], k: usize) -> f64 {
    if k == 0 {
        return 0.0;
    }
    intersection_count(relevant, actual) as f64 / k as f64
}

/// Precision@k: fraction of the returned results that are relevant.
///
/// Returns `|relevant ∩ actual| / |actual|`.
pub fn precision(relevant: &[i64], actual: &[i64]) -> f64 {
    if actual.is_empty() {
        return 0.0;
    }
    intersection_count(relevant, actual) as f64 / actual.len() as f64
}

/// F1@k: harmonic mean of recall and precision.
///
/// `F1 = 2 · (recall · precision) / (recall + precision)`
pub fn f1(relevant: &[i64], actual: &[i64], k: usize) -> f64 {
    let r = recall(relevant, actual, k);
    let p = precision(relevant, actual);
    if r + p == 0.0 {
        return 0.0;
    }
    2.0 * r * p / (r + p)
}

/// Reciprocal rank: `1 / (position of first relevant result + 1)`.
///
/// `actual` must be in result order (not sorted). Scans linearly and
/// returns `1/(i+1)` for the first element found in `relevant`.
/// Returns 0.0 if no relevant item appears in `actual`.
pub fn reciprocal_rank(relevant: &[i64], actual: &[i64]) -> f64 {
    let relevant_set: HashSet<i64> = relevant.iter().copied().collect();
    for (i, &item) in actual.iter().enumerate() {
        if relevant_set.contains(&item) {
            return 1.0 / (i as f64 + 1.0);
        }
    }
    0.0
}

/// Average precision: mean precision at each relevant position.
///
/// `actual` must be in result order (not sorted). For each position i
/// where `actual[i]` is in `relevant`:
///   `precision_at_i = (relevant items seen so far) / (i + 1)`
///
/// AP = sum of all `precision_at_i` / `|relevant|`.
pub fn average_precision(relevant: &[i64], actual: &[i64]) -> f64 {
    if relevant.is_empty() {
        return 0.0;
    }
    let relevant_set: HashSet<i64> = relevant.iter().copied().collect();
    let mut hits = 0u64;
    let mut sum = 0.0f64;
    for (i, &item) in actual.iter().enumerate() {
        if relevant_set.contains(&item) {
            hits += 1;
            sum += hits as f64 / (i as f64 + 1.0);
        }
    }
    if hits == 0 {
        0.0
    } else {
        sum / relevant.len() as f64
    }
}

/// NDCG@k — normalized discounted cumulative gain, **standard form**:
/// gain is positioned by the item's RESULT rank, with binary relevance
/// (membership in `relevant`).
///
/// `actual` must be in result order (not sorted). Each item of
/// `actual` that appears in `relevant` contributes `1/log2(i+2)` at
/// its own rank `i` — first occurrence only, so duplicate ids in a
/// result cannot inflate the score past 1.0. The ideal DCG places all
/// `|relevant|` items at the top ranks: `Σ_{i<|relevant|} 1/log2(i+2)`.
///
/// Composition with the k-recall@r window (the validation layer
/// truncates `relevant` to k and `actual` to r ≥ k): the gain sum
/// runs over the full retrieved window against the k-truncated ground
/// truth, so a relevant item recovered deep in the r-window earns its
/// (heavily discounted) gain instead of vanishing — the same
/// rationale as the k-recall@r denominator fix — and `ndcg ≤ 1`
/// always holds because late ranks discount below every ideal slot.
///
/// Deviation note: at least one external benchmark's "NDCG" positions
/// gain by the id's rank in the GROUND TRUTH rather than the result —
/// a different formula rewarding retrieval of well-ranked GT items
/// regardless of returned order. This is the standard form; comparing
/// scores across the two requires labeling, never blending.
pub fn ndcg(relevant: &[i64], actual: &[i64]) -> f64 {
    if relevant.is_empty() {
        return 0.0;
    }
    let mut remaining: HashSet<i64> = relevant.iter().copied().collect();
    let mut dcg = 0.0f64;
    for (i, &item) in actual.iter().enumerate() {
        if remaining.remove(&item) {
            dcg += 1.0 / (i as f64 + 2.0).log2();
        }
    }
    let idcg: f64 = (0..relevant.len())
        .map(|i| 1.0 / (i as f64 + 2.0).log2())
        .sum();
    if idcg == 0.0 { 0.0 } else { dcg / idcg }
}

/// Truncate a slice to at most `k` elements and return a sorted copy.
pub fn truncate_and_sort(items: &[i64], k: usize) -> Vec<i64> {
    let end = items.len().min(k);
    let mut v = items[..end].to_vec();
    v.sort_unstable();
    v
}

/// Which relevancy function to compute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelevancyFn {
    Recall,
    Precision,
    F1,
    ReciprocalRank,
    AveragePrecision,
    Ndcg,
}

impl RelevancyFn {
    /// Parse from a string name (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "recall" => Some(Self::Recall),
            "precision" => Some(Self::Precision),
            "f1" => Some(Self::F1),
            "reciprocal_rank" | "reciprocalrank" | "mrr" => Some(Self::ReciprocalRank),
            "average_precision" | "averageprecision" | "ap" | "map" => Some(Self::AveragePrecision),
            "ndcg" => Some(Self::Ndcg),
            _ => None,
        }
    }

    /// Metric name for display and histogram keys.
    pub fn metric_name(&self) -> &'static str {
        match self {
            Self::Recall => "recall",
            Self::Precision => "precision",
            Self::F1 => "f1",
            Self::ReciprocalRank => "reciprocal_rank",
            Self::AveragePrecision => "average_precision",
            Self::Ndcg => "ndcg",
        }
    }

    /// Compute this function's score.
    ///
    /// For recall, precision, and F1: `relevant` and `actual` should be
    /// sorted slices (pre-truncated to k). For reciprocal_rank and
    /// average_precision: `actual_ordered` is the original result order.
    pub fn compute(
        &self,
        relevant_sorted: &[i64],
        actual_sorted: &[i64],
        actual_ordered: &[i64],
        k: usize,
    ) -> f64 {
        match self {
            Self::Recall => recall(relevant_sorted, actual_sorted, k),
            Self::Precision => precision(relevant_sorted, actual_sorted),
            Self::F1 => f1(relevant_sorted, actual_sorted, k),
            Self::ReciprocalRank => reciprocal_rank(relevant_sorted, actual_ordered),
            Self::AveragePrecision => average_precision(relevant_sorted, actual_ordered),
            Self::Ndcg => ndcg(relevant_sorted, actual_ordered),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersection_count_basic() {
        assert_eq!(intersection_count(&[1, 3, 5, 7], &[2, 3, 5, 8]), 2);
        assert_eq!(intersection_count(&[1, 2, 3], &[1, 2, 3]), 3);
        assert_eq!(intersection_count(&[1, 2], &[3, 4]), 0);
        assert_eq!(intersection_count(&[], &[1, 2, 3]), 0);
        assert_eq!(intersection_count(&[1], &[]), 0);
    }

    #[test]
    fn recall_perfect() {
        // All 5 relevant items found in actual
        let relevant = vec![1, 2, 3, 4, 5];
        let actual = vec![1, 2, 3, 4, 5];
        assert!((recall(&relevant, &actual, 5) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn recall_partial() {
        // 3 of 5 relevant items found
        let relevant = vec![1, 2, 3, 4, 5];
        let actual = vec![1, 3, 5, 6, 7];
        assert!((recall(&relevant, &actual, 5) - 0.6).abs() < 1e-10);
    }

    #[test]
    fn recall_k_at_r_divides_by_k_not_r() {
        // k-recall@r: retrieved 10x more candidates than k, all
        // 10 ground-truth items live somewhere in the wider r=64
        // returned window. Recall should be 1.0 (10 of 10 found),
        // *not* 10/64 = 0.156.
        //
        // This is the formula pin for the validation-layer fix —
        // when the call site truncates `actual` to r (not k),
        // intersection_count / k yields the right number.
        let relevant_topk = vec![100, 200, 300, 400, 500, 600, 700, 800, 900, 1000];
        // Returned: first 10 are post-filter noise (none of the
        // GT). The 10 GT items live at ranks 10..20 of the
        // returned set (still inside r=64). This is the realistic
        // "post-filter ANN+predicate found the right items but
        // only after a flock of pre-filter survivors" shape.
        let mut returned: Vec<i64> = (1i64..=64).map(|n| n * 13 + 1_000_000).collect();
        for (i, gt) in relevant_topk.iter().enumerate() {
            returned[10 + i] = *gt;
        }
        let actual_topr = truncate_and_sort(&returned, 64);
        let expected_topk = truncate_and_sort(&relevant_topk, 10);
        // Divide by k=10, intersect over the r=64 returned set.
        let r = recall(&expected_topk, &actual_topr, 10);
        assert!(
            (r - 1.0).abs() < 1e-10,
            "expected recall 1.0 (all 10 GT found in r=64), got {r}"
        );

        // The "bug shape" — truncating actual to k (not r) when
        // most of the top-k returned is post-filter noise. With
        // 1 of the GT in the top-k returned, recall collapses
        // to 0.1 regardless of how many were in the wider r.
        let actual_topk_buggy = truncate_and_sort(&returned, 10);
        let r_buggy = recall(&expected_topk, &actual_topk_buggy, 10);
        assert!(
            r_buggy < 0.2,
            "buggy form should produce ~K/R-shaped low recall, got {r_buggy}"
        );
    }

    #[test]
    fn recall_zero() {
        let relevant = vec![1, 2, 3];
        let actual = vec![4, 5, 6];
        assert!((recall(&relevant, &actual, 3)).abs() < 1e-10);
    }

    #[test]
    fn recall_k_zero() {
        assert_eq!(recall(&[1, 2], &[1, 2], 0), 0.0);
    }

    #[test]
    fn precision_perfect() {
        let relevant = vec![1, 2, 3, 4, 5];
        let actual = vec![1, 2, 3, 4, 5];
        assert!((precision(&relevant, &actual) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn precision_half() {
        // 2 of 4 actual items are relevant
        let relevant = vec![1, 2, 3, 4, 5];
        let actual = vec![1, 3, 6, 7];
        assert!((precision(&relevant, &actual) - 0.5).abs() < 1e-10);
    }

    #[test]
    fn precision_empty_actual() {
        assert_eq!(precision(&[1, 2, 3], &[]), 0.0);
    }

    #[test]
    fn f1_perfect() {
        let relevant = vec![1, 2, 3];
        let actual = vec![1, 2, 3];
        assert!((f1(&relevant, &actual, 3) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn f1_zero_when_no_overlap() {
        let relevant = vec![1, 2, 3];
        let actual = vec![4, 5, 6];
        assert!((f1(&relevant, &actual, 3)).abs() < 1e-10);
    }

    #[test]
    fn f1_known_value() {
        // recall = 2/5 = 0.4, precision = 2/3 = 0.667
        // f1 = 2 * 0.4 * 0.667 / (0.4 + 0.667) = 0.5
        let relevant = vec![1, 2, 3, 4, 5];
        let actual = vec![1, 3, 6];
        let score = f1(&relevant, &actual, 5);
        assert!((score - 0.5).abs() < 0.01, "f1={score}");
    }

    #[test]
    fn reciprocal_rank_first_position() {
        // First result is relevant → RR = 1.0
        let relevant = vec![1, 2, 3];
        let actual = vec![1, 5, 6, 7];
        assert!((reciprocal_rank(&relevant, &actual) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn reciprocal_rank_third_position() {
        // First relevant at position 2 (0-indexed) → RR = 1/3
        let relevant = vec![1, 2, 3];
        let actual = vec![5, 6, 2, 7];
        assert!((reciprocal_rank(&relevant, &actual) - 1.0 / 3.0).abs() < 1e-10);
    }

    #[test]
    fn reciprocal_rank_no_match() {
        let relevant = vec![1, 2, 3];
        let actual = vec![4, 5, 6];
        assert_eq!(reciprocal_rank(&relevant, &actual), 0.0);
    }

    #[test]
    fn average_precision_perfect_order() {
        // All relevant, in order: AP@3 = (1/1 + 2/2 + 3/3) / 3 = 1.0
        let relevant = vec![1, 2, 3];
        let actual = vec![1, 2, 3];
        assert!((average_precision(&relevant, &actual) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn average_precision_interleaved() {
        // relevant = [1,2,3], actual = [1, 4, 2, 5, 3]
        // pos 0: hit(1), prec=1/1=1.0
        // pos 1: miss
        // pos 2: hit(2), prec=2/3=0.667
        // pos 3: miss
        // pos 4: hit(3), prec=3/5=0.6
        // AP = (1.0 + 0.667 + 0.6) / 3 = 0.756
        let relevant = vec![1, 2, 3];
        let actual = vec![1, 4, 2, 5, 3];
        let ap = average_precision(&relevant, &actual);
        assert!((ap - 0.756).abs() < 0.01, "ap={ap}");
    }

    #[test]
    fn average_precision_no_hits() {
        let relevant = vec![1, 2, 3];
        let actual = vec![4, 5, 6];
        assert_eq!(average_precision(&relevant, &actual), 0.0);
    }

    #[test]
    fn average_precision_empty_relevant() {
        assert_eq!(average_precision(&[], &[1, 2, 3]), 0.0);
    }

    #[test]
    fn truncate_and_sort_basic() {
        let items = vec![5, 3, 1, 4, 2, 10, 8];
        let result = truncate_and_sort(&items, 4);
        assert_eq!(result, vec![1, 3, 4, 5]);
    }

    #[test]
    fn truncate_and_sort_k_larger_than_len() {
        let items = vec![3, 1, 2];
        let result = truncate_and_sort(&items, 100);
        assert_eq!(result, vec![1, 2, 3]);
    }

    #[test]
    fn ndcg_perfect_order_is_one() {
        // All relevant items at the top ranks = the ideal arrangement.
        let relevant = vec![1, 2, 3];
        let actual = vec![1, 2, 3, 7, 8];
        assert!((ndcg(&relevant, &actual) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn ndcg_rank_position_discounts_by_result_rank() {
        // One relevant item of one: at rank 0 → 1.0; at rank 2 →
        // (1/log2(4)) / (1/log2(2)) = 0.5. RESULT rank drives the
        // discount — the standard-form pin.
        let relevant = vec![9];
        assert!((ndcg(&relevant, &[9, 4, 5]) - 1.0).abs() < 1e-10);
        assert!((ndcg(&relevant, &[4, 5, 9]) - 0.5).abs() < 1e-10);
    }

    #[test]
    fn ndcg_known_interleaved_value() {
        // relevant = [1,2], actual = [1, 7, 2]:
        // dcg  = 1/log2(2) + 1/log2(4) = 1.0 + 0.5 = 1.5
        // idcg = 1/log2(2) + 1/log2(3) = 1.0 + 0.63093 = 1.63093
        // ndcg = 1.5 / 1.63093 = 0.91972
        let score = ndcg(&[1, 2], &[1, 7, 2]);
        assert!((score - 0.91972).abs() < 1e-4, "ndcg={score}");
    }

    #[test]
    fn ndcg_duplicate_results_do_not_inflate() {
        // The same relevant id returned twice earns gain ONCE (first
        // occurrence) — a duplicated row cannot push ndcg past 1.0.
        let relevant = vec![1];
        let score = ndcg(&relevant, &[1, 1, 1]);
        assert!((score - 1.0).abs() < 1e-10, "ndcg={score}");
    }

    #[test]
    fn ndcg_no_hits_and_empty_relevant_are_zero() {
        assert_eq!(ndcg(&[1, 2, 3], &[4, 5, 6]), 0.0);
        assert_eq!(ndcg(&[], &[1, 2, 3]), 0.0);
    }

    #[test]
    fn ndcg_deep_r_window_recovery_stays_bounded() {
        // k-recall@r composition: k=2 GT items recovered deep in an
        // r=8 window still earn (discounted) gain, and the score
        // stays strictly inside (0, 1).
        let relevant = vec![100, 200];
        let actual = vec![1, 2, 3, 4, 5, 6, 100, 200];
        let score = ndcg(&relevant, &actual);
        assert!(score > 0.0 && score < 1.0, "ndcg={score}");
    }

    #[test]
    fn relevancy_fn_parse() {
        assert_eq!(RelevancyFn::parse("recall"), Some(RelevancyFn::Recall));
        assert_eq!(
            RelevancyFn::parse("PRECISION"),
            Some(RelevancyFn::Precision)
        );
        assert_eq!(RelevancyFn::parse("f1"), Some(RelevancyFn::F1));
        assert_eq!(
            RelevancyFn::parse("reciprocal_rank"),
            Some(RelevancyFn::ReciprocalRank)
        );
        assert_eq!(RelevancyFn::parse("mrr"), Some(RelevancyFn::ReciprocalRank));
        assert_eq!(
            RelevancyFn::parse("average_precision"),
            Some(RelevancyFn::AveragePrecision)
        );
        assert_eq!(
            RelevancyFn::parse("ap"),
            Some(RelevancyFn::AveragePrecision)
        );
        assert_eq!(
            RelevancyFn::parse("map"),
            Some(RelevancyFn::AveragePrecision)
        );
        assert_eq!(RelevancyFn::parse("ndcg"), Some(RelevancyFn::Ndcg));
        assert_eq!(RelevancyFn::parse("NDCG"), Some(RelevancyFn::Ndcg));
        assert_eq!(RelevancyFn::parse("unknown"), None);
    }

    #[test]
    fn relevancy_fn_compute_dispatch() {
        let relevant = vec![1, 2, 3, 4, 5];
        let actual_sorted = vec![1, 2, 3, 6, 7];
        let actual_ordered = vec![1, 6, 2, 7, 3];

        let r = RelevancyFn::Recall.compute(&relevant, &actual_sorted, &actual_ordered, 5);
        assert!((r - 0.6).abs() < 1e-10);

        let p = RelevancyFn::Precision.compute(&relevant, &actual_sorted, &actual_ordered, 5);
        assert!((p - 0.6).abs() < 1e-10);

        let rr = RelevancyFn::ReciprocalRank.compute(&relevant, &actual_sorted, &actual_ordered, 5);
        assert!((rr - 1.0).abs() < 1e-10); // first item in actual_ordered is relevant
    }
}
