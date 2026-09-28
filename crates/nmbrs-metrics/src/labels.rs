// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Dimensional metric labels.
//!
//! Every metric carries a set of key-value labels for identification.
//! Labels are immutable, `Arc`-shared for cheap cloning, and compose
//! hierarchically (child inherits parent).

use std::fmt;
use std::sync::Arc;

/// An immutable set of key-value label pairs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Labels {
    pairs: Arc<Vec<(String, String)>>,
}

impl Default for Labels {
    fn default() -> Self {
        Self::empty()
    }
}

impl Labels {
    pub fn empty() -> Self {
        Self {
            pairs: Arc::new(Vec::new()),
        }
    }

    pub fn of(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            pairs: Arc::new(vec![(key.into(), value.into())]),
        }
    }

    pub fn with(&self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let mut pairs = (*self.pairs).clone();
        let key = key.into();
        if let Some(pos) = pairs.iter().position(|(k, _)| k == &key) {
            pairs[pos].1 = value.into();
        } else {
            pairs.push((key, value.into()));
        }
        Self {
            pairs: Arc::new(pairs),
        }
    }

    pub fn extend(&self, child: &Labels) -> Labels {
        let mut pairs = (*self.pairs).clone();
        for (k, v) in child.pairs.iter() {
            if let Some(pos) = pairs.iter().position(|(pk, _)| pk == k) {
                pairs[pos].1 = v.clone();
            } else {
                pairs.push((k.clone(), v.clone()));
            }
        }
        Labels {
            pairs: Arc::new(pairs),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn len(&self) -> usize {
        self.pairs.len()
    }
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.pairs.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn to_prometheus(&self) -> String {
        if self.pairs.is_empty() {
            return String::new();
        }
        let inner: Vec<String> = self
            .pairs
            .iter()
            .map(|(k, v)| format!("{k}=\"{v}\""))
            .collect();
        format!("{{{}}}", inner.join(","))
    }

    pub fn to_dotted(&self) -> String {
        self.pairs
            .iter()
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
            .join(".")
    }

    /// Content-defined identity hash for this label set.
    ///
    /// Stability contract:
    ///   * Order-independent — pairs are sorted by key before
    ///     hashing, so `Labels::of("a","1").with("b","2")` and
    ///     `Labels::of("b","2").with("a","1")` hash to the same
    ///     value. Without this, two code paths that construct
    ///     "the same" label set in different orders create two
    ///     distinct `metric_instance` rows in the sqlite sink —
    ///     a silent double-count on every aggregate query.
    ///   * Uses FNV-1a 64 (defined inline). `DefaultHasher`
    ///     (SipHasher13) is deterministic within a Rust version
    ///     but the std docs explicitly reserve the right to
    ///     change it — anything that writes a hash to durable
    ///     storage needs a hasher that we own.
    pub fn identity_hash(&self) -> u64 {
        // Sort by key (then value for total order) without
        // mutating the Arc'd vec. Borrowed view is cheap; the
        // pair count is small (typically <16).
        let mut sorted: Vec<(&str, &str)> = self
            .pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        sorted.sort();
        let mut h: u64 = 0xcbf29ce484222325; // FNV-1a 64-bit offset
        for (k, v) in &sorted {
            for b in k.as_bytes() {
                h ^= *b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            // `=` separator so `a=bc` and `ab=c` don't collide.
            h ^= b'=' as u64;
            h = h.wrapping_mul(0x100000001b3);
            for b in v.as_bytes() {
                h ^= *b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            // `\0` separator between pairs.
            h ^= 0;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    /// Canonical sorted-by-key view of the pairs, borrowed.
    /// Used by reporters that need a stable serialised form
    /// (filename, db spec, etc.) so the on-disk identity
    /// matches `identity_hash`.
    pub fn sorted_pairs(&self) -> Vec<(&str, &str)> {
        let mut v: Vec<(&str, &str)> = self
            .pairs
            .iter()
            .map(|(k, val)| (k.as_str(), val.as_str()))
            .collect();
        v.sort();
        v
    }

    /// Build the OpenMetrics-canonical sample identifier for
    /// this label set under the given metric family name —
    /// the text form used by Prometheus, OpenMetrics, and
    /// VictoriaMetrics for uniquely naming a time series.
    ///
    /// Shape: `metric_name{key="value",key="value"}`
    ///   * Labels are sorted by name (canonical: two label
    ///     dicts that are equal as a mapping produce equal
    ///     spec text).
    ///   * `__name__` is excluded from the labels block (the
    ///     metric name is the prefix, per spec); other code
    ///     paths still see `__name__` as a regular label row
    ///     in the database for query uniformity.
    ///   * Empty label values are dropped (OpenMetrics spec
    ///     §"Label": "Empty label values SHOULD be treated as
    ///     if the label was not present").
    ///   * Values are escaped per OpenMetrics §"Escaping":
    ///     `\` → `\\`, `"` → `\"`, `\n` → `\n` literal.
    pub fn to_canonical_spec(&self, metric_name: &str) -> String {
        let mut pairs: Vec<(&str, &str)> = self
            .pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .filter(|(k, v)| !v.is_empty() && *k != "__name__")
            .collect();
        pairs.sort();
        let mut out = String::with_capacity(metric_name.len() + 32);
        out.push_str(metric_name);
        out.push('{');
        let mut first = true;
        for (k, v) in pairs {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(k);
            out.push_str("=\"");
            escape_label_value_into(&mut out, v);
            out.push('"');
        }
        out.push('}');
        out
    }
}

/// Escape a label value per OpenMetrics §"Escaping" rules:
///   `\`  → `\\`
///   `"`  → `\"`
///   `\n` → `\n` (two-char literal backslash-n)
/// Other characters pass through unchanged.
pub fn escape_label_value_into(out: &mut String, v: &str) {
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

/// Convenience: standalone form of [`escape_label_value_into`]
/// for callers that don't already have a buffer.
pub fn escape_label_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    escape_label_value_into(&mut out, v);
    out
}

impl fmt::Display for Labels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_prometheus())
    }
}

/// Semantic category for metric filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetricCategory {
    Core,
    Progress,
    Errors,
    Driver,
    Internals,
    Verification,
    Config,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_with_override() {
        let l = Labels::of("a", "1").with("a", "2");
        assert_eq!(l.get("a"), Some("2"));
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn labels_extend_child_wins() {
        let parent = Labels::of("session", "s1").with("activity", "write");
        let child = Labels::of("name", "timer1").with("activity", "read");
        let merged = parent.extend(&child);
        assert_eq!(merged.get("session"), Some("s1"));
        assert_eq!(merged.get("activity"), Some("read"));
        assert_eq!(merged.get("name"), Some("timer1"));
    }

    #[test]
    fn labels_prometheus_format() {
        let l = Labels::of("session", "abc").with("name", "ops_total");
        assert_eq!(l.to_prometheus(), r#"{session="abc",name="ops_total"}"#);
    }

    #[test]
    fn identity_hash_order_independent() {
        // Stability contract: two code paths that construct
        // the same logical label set in different orders MUST
        // hash equally. Otherwise downstream reporters create
        // duplicate `metric_instance` rows for the same
        // logical instance.
        let a = Labels::of("k", "1")
            .with("optimize_for", "recall")
            .with("phase", "ann_query");
        let b = Labels::of("phase", "ann_query")
            .with("k", "1")
            .with("optimize_for", "recall");
        let c = Labels::of("optimize_for", "recall")
            .with("phase", "ann_query")
            .with("k", "1");
        assert_eq!(a.identity_hash(), b.identity_hash());
        assert_eq!(a.identity_hash(), c.identity_hash());
    }

    #[test]
    fn identity_hash_distinguishes_distinct_sets() {
        let a = Labels::of("phase", "ann_query");
        let b = Labels::of("phase", "pvs_query");
        assert_ne!(a.identity_hash(), b.identity_hash());
    }

    #[test]
    fn identity_hash_no_collision_between_split_keys() {
        // Defensive: ensure the `=`/`\0` separators stop
        // `ab=c` colliding with `a=bc`.
        let a = Labels::of("ab", "c");
        let b = Labels::of("a", "bc");
        assert_ne!(a.identity_hash(), b.identity_hash());
    }

    #[test]
    fn canonical_spec_sorts_and_quotes() {
        let l = Labels::of("phase", "ann_query")
            .with("k", "1")
            .with("optimize_for", "recall");
        assert_eq!(
            l.to_canonical_spec("recall_mean"),
            r#"recall_mean{k="1",optimize_for="recall",phase="ann_query"}"#,
        );
    }

    #[test]
    fn canonical_spec_order_independent() {
        let a = Labels::of("phase", "ann").with("k", "1");
        let b = Labels::of("k", "1").with("phase", "ann");
        assert_eq!(
            a.to_canonical_spec("recall_mean"),
            b.to_canonical_spec("recall_mean"),
        );
    }

    #[test]
    fn canonical_spec_drops_empty_values_and_underscore_name() {
        let l = Labels::of("phase", "ann")
            .with("hint", "")
            .with("__name__", "should_be_ignored_here");
        let spec = l.to_canonical_spec("ops_total");
        // Empty value `hint=""` dropped (OpenMetrics §"Label"
        // empty-value clause); `__name__` excluded from
        // labels block.
        assert_eq!(spec, r#"ops_total{phase="ann"}"#);
    }

    #[test]
    fn canonical_spec_escapes_values() {
        let l = Labels::of("note", r#"has "quotes" and \ slash"#);
        assert_eq!(
            l.to_canonical_spec("m"),
            r#"m{note="has \"quotes\" and \\ slash"}"#,
        );
    }

    #[test]
    fn canonical_spec_empty_labels() {
        assert_eq!(
            Labels::empty().to_canonical_spec("ops_total"),
            "ops_total{}"
        );
    }

    #[test]
    fn labels_clone_shares_arc() {
        let l = Labels::of("a", "1").with("b", "2");
        let l2 = l.clone();
        assert!(Arc::ptr_eq(&l.pairs, &l2.pairs));
    }
}
