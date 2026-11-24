// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! "Did you mean …?" suggestions for unrecognised names: one edit-distance
//! rule shared by every diagnostic that names a closest candidate (CLI
//! parameters and flags, wrapper names).

/// The candidate closest to `query` by Levenshtein edit distance, or `None`
/// when even the closest is more than half the query's length away (at least
/// 2 edits are always allowed), since beyond that the suggestion is noise.
pub fn closest_match<'a>(
    query: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let limit = (query.chars().count() / 2).max(2);
    let mut best: Option<(&str, usize)> = None;
    for c in candidates {
        let d = levenshtein(query, c);
        match best {
            Some((_, prev)) if d >= prev => {}
            _ => best = Some((c, d)),
        }
    }
    best.filter(|&(_, d)| d <= limit).map(|(s, _)| s)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr: Vec<usize> = vec![0; m + 1];
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("validate", "validatte"), 1);
    }

    #[test]
    fn closest_match_finds_typo() {
        let names = ["validate", "poll", "delay"];
        assert_eq!(closest_match("validatte", names), Some("validate"));
        assert_eq!(closest_match("plll", names), Some("poll"));
        assert_eq!(closest_match("wildly_different", names), None);
    }
}
