// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Op sequencing: maps cycles to op templates via a ratio-based LUT.
//!
//! Three sequencing strategies arrange ops within a stanza (one
//! complete rotation through the sequence):
//! - **Bucket** (default): interleaved round-robin from ratio buckets
//! - **Interval**: evenly spaced by frequency across the stanza
//! - **Concat**: all of first op, then all of second, etc.

use nmbrs_workload::model::ParsedOp;

/// Sequencing strategy for arranging ops within a stanza.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SequencerType {
    /// Round-robin from ratio-sized buckets. Default.
    Bucket,
    /// Evenly spaced across the stanza by frequency.
    Interval,
    /// All of first, then all of second, etc.
    Concat,
}

impl SequencerType {
    /// Parse from string parameter value.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "bucket" => Ok(Self::Bucket),
            "interval" => Ok(Self::Interval),
            "concat" => Ok(Self::Concat),
            other => Err(format!(
                "unknown sequencer type: '{other}', use bucket|interval|concat"
            )),
        }
    }
}

/// Maps cycle numbers to op templates using a pre-computed LUT.
///
/// The LUT length equals the sum of all ratios (the "stanza length").
/// `cycle % stanza_length` indexes into the LUT to select an op.
pub struct OpSequence {
    ops: Vec<ParsedOp>,
    lut: Vec<usize>,
    sequencer_type: SequencerType,
}

impl OpSequence {
    /// Build a sequence from ops with explicit ratios and strategy.
    pub fn build(ops: Vec<ParsedOp>, ratios: &[u64], strategy: SequencerType) -> Self {
        assert_eq!(
            ops.len(),
            ratios.len(),
            "ops and ratios must have equal length"
        );
        assert!(!ops.is_empty(), "must have at least one op");

        let lut = match strategy {
            SequencerType::Bucket => build_bucket_lut(ratios),
            SequencerType::Interval => build_interval_lut(ratios),
            SequencerType::Concat => build_concat_lut(ratios),
        };

        Self {
            ops,
            lut,
            sequencer_type: strategy,
        }
    }

    /// Build with uniform ratios (1 each) using bucket sequencing.
    pub fn uniform(ops: Vec<ParsedOp>) -> Self {
        let ratios = vec![1u64; ops.len()];
        Self::build(ops, &ratios, SequencerType::Bucket)
    }

    /// Build from ops, extracting ratios from the `ratio` param
    /// on each op template (defaults to 1).
    /// Build from ops, extracting ratios from the `ratio` field.
    ///
    /// Checks `params["ratio"]` first (explicit params), then
    /// `op["ratio"]` (inline with op fields). Defaults to 1.
    pub fn from_ops(ops: Vec<ParsedOp>, strategy: SequencerType) -> Self {
        let ratios: Vec<u64> = ops
            .iter()
            .map(|op| {
                op.params
                    .get("ratio")
                    .and_then(|v| v.as_u64())
                    .or_else(|| op.op.get("ratio").and_then(|v| v.as_u64()))
                    .unwrap_or(1)
            })
            .collect();
        Self::build(ops, &ratios, strategy)
    }

    /// Get the op template for a given cycle.
    #[inline]
    pub fn get(&self, cycle: u64) -> &ParsedOp {
        let idx = self.lut[(cycle as usize) % self.lut.len()];
        &self.ops[idx]
    }

    /// Get the op template and its index for a given cycle.
    #[inline]
    pub fn get_with_index(&self, cycle: u64) -> (usize, &ParsedOp) {
        let idx = self.lut[(cycle as usize) % self.lut.len()];
        (idx, &self.ops[idx])
    }

    /// All unique op templates in declaration order.
    pub fn templates(&self) -> &[ParsedOp] {
        &self.ops
    }

    /// Number of distinct op templates.
    pub fn op_count(&self) -> usize {
        self.ops.len()
    }

    /// Stanza length (sum of all ratios = LUT length).
    pub fn stanza_length(&self) -> usize {
        self.lut.len()
    }

    /// Get all ops in a stanza starting at the given cycle.
    ///
    /// Returns (op_template, cycle_within_stanza) pairs for each
    /// position in the stanza. Used by the capture-aware executor
    /// to process a full stanza as a unit.
    pub fn stanza_ops(&self, base_cycle: u64) -> Vec<(&ParsedOp, u64)> {
        (0..self.lut.len())
            .map(|offset| {
                let cycle = base_cycle + offset as u64;
                (self.get(cycle), cycle)
            })
            .collect()
    }

    /// The sequencing strategy used.
    pub fn sequencer_type(&self) -> SequencerType {
        self.sequencer_type
    }

    /// The raw LUT (for inspection/testing).
    pub fn lut(&self) -> &[usize] {
        &self.lut
    }
}

// =================================================================
// Sequencer implementations
// =================================================================

/// Bucket: round-robin from ratio-sized buckets.
///
/// Each op gets a "bucket" with `ratio` tokens. We cycle through
/// all non-empty buckets, drawing one token per pass, until all
/// buckets are empty.
///
/// Example: A:3, B:2, C:1
///   Pass 1: A, B, C  (all have tokens)
///   Pass 2: A, B     (C exhausted)
///   Pass 3: A        (B exhausted)
///   LUT: [0, 1, 2, 0, 1, 0]
fn build_bucket_lut(ratios: &[u64]) -> Vec<usize> {
    let total: u64 = ratios.iter().sum();
    let mut lut = Vec::with_capacity(total as usize);
    let mut remaining: Vec<u64> = ratios.to_vec();

    loop {
        let mut any_drawn = false;
        for (i, rem) in remaining.iter_mut().enumerate() {
            if *rem > 0 {
                lut.push(i);
                *rem -= 1;
                any_drawn = true;
            }
        }
        if !any_drawn {
            break;
        }
    }

    lut
}

/// Interval: evenly spaced across the stanza.
///
/// Each op is placed at positions determined by its frequency
/// (ratio / total). Ops with higher ratios appear more frequently
/// and are more evenly distributed.
///
/// Example: A:4, B:2
///   Total = 6. A at every 1.5, B at every 3.
///   Positions sorted: A(0), B(0), A(1.5), A(3.0), B(3.0), A(4.5)
///   LUT: [0, 1, 0, 0, 1, 0]
fn build_interval_lut(ratios: &[u64]) -> Vec<usize> {
    let total: u64 = ratios.iter().sum();
    if total == 0 {
        return Vec::new();
    }

    // Generate (position, op_index) pairs
    let mut entries: Vec<(f64, usize, usize)> = Vec::new(); // (pos, op_idx, instance)
    for (i, &ratio) in ratios.iter().enumerate() {
        if ratio == 0 {
            continue;
        }
        let spacing = total as f64 / ratio as f64;
        for j in 0..ratio as usize {
            entries.push((j as f64 * spacing, i, j));
        }
    }

    // Sort by position, then by op index for stability
    entries.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });

    entries.iter().map(|&(_, idx, _)| idx).collect()
}

/// Concat: all of first op, then all of second, etc.
///
/// Example: A:3, B:2, C:1
///   LUT: [0, 0, 0, 1, 1, 2]
fn build_concat_lut(ratios: &[u64]) -> Vec<usize> {
    let mut lut = Vec::new();
    for (i, &ratio) in ratios.iter().enumerate() {
        for _ in 0..ratio {
            lut.push(i);
        }
    }
    lut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(seq: &OpSequence, count: usize) -> Vec<String> {
        (0..count as u64).map(|c| seq.get(c).name.clone()).collect()
    }

    // --- Bucket tests ---

    #[test]
    fn bucket_uniform() {
        let ops = vec![
            ParsedOp::simple("A", "a"),
            ParsedOp::simple("B", "b"),
            ParsedOp::simple("C", "c"),
        ];
        let seq = OpSequence::build(ops, &[1, 1, 1], SequencerType::Bucket);
        assert_eq!(seq.stanza_length(), 3);
        assert_eq!(names(&seq, 6), vec!["A", "B", "C", "A", "B", "C"]);
    }

    #[test]
    fn bucket_weighted() {
        let ops = vec![
            ParsedOp::simple("R", "read"),
            ParsedOp::simple("W", "write"),
            ParsedOp::simple("D", "delete"),
        ];
        let seq = OpSequence::build(ops, &[3, 2, 1], SequencerType::Bucket);
        assert_eq!(seq.stanza_length(), 6);
        let n = names(&seq, 6);
        assert_eq!(n.iter().filter(|s| *s == "R").count(), 3);
        assert_eq!(n.iter().filter(|s| *s == "W").count(), 2);
        assert_eq!(n.iter().filter(|s| *s == "D").count(), 1);
    }

    #[test]
    fn bucket_interleaves() {
        let ops = vec![ParsedOp::simple("A", "a"), ParsedOp::simple("B", "b")];
        let seq = OpSequence::build(ops, &[3, 1], SequencerType::Bucket);
        // Bucket draws: Pass1: A,B  Pass2: A  Pass3: A
        assert_eq!(seq.lut(), &[0, 1, 0, 0]);
    }

    // --- Interval tests ---

    #[test]
    fn interval_uniform() {
        let ops = vec![ParsedOp::simple("A", "a"), ParsedOp::simple("B", "b")];
        let seq = OpSequence::build(ops, &[1, 1], SequencerType::Interval);
        assert_eq!(seq.stanza_length(), 2);
        assert_eq!(names(&seq, 4), vec!["A", "B", "A", "B"]);
    }

    #[test]
    fn interval_weighted() {
        let ops = vec![ParsedOp::simple("A", "a"), ParsedOp::simple("B", "b")];
        let seq = OpSequence::build(ops, &[4, 2], SequencerType::Interval);
        assert_eq!(seq.stanza_length(), 6);
        let n = names(&seq, 6);
        assert_eq!(n.iter().filter(|s| *s == "A").count(), 4);
        assert_eq!(n.iter().filter(|s| *s == "B").count(), 2);
    }

    #[test]
    fn interval_distributes_evenly() {
        let ops = vec![ParsedOp::simple("A", "a"), ParsedOp::simple("B", "b")];
        let seq = OpSequence::build(ops, &[3, 3], SequencerType::Interval);
        // Both at spacing 2: A at 0,2,4; B at 0,2,4
        // Sorted by pos then index: A(0), B(0), A(2), B(2), A(4), B(4)
        assert_eq!(seq.stanza_length(), 6);
        assert_eq!(names(&seq, 6), vec!["A", "B", "A", "B", "A", "B"]);
    }

    // --- Concat tests ---

    #[test]
    fn concat_sequential() {
        let ops = vec![
            ParsedOp::simple("A", "a"),
            ParsedOp::simple("B", "b"),
            ParsedOp::simple("C", "c"),
        ];
        let seq = OpSequence::build(ops, &[3, 2, 1], SequencerType::Concat);
        assert_eq!(seq.stanza_length(), 6);
        assert_eq!(names(&seq, 6), vec!["A", "A", "A", "B", "B", "C"]);
    }

    #[test]
    fn concat_wraps() {
        let ops = vec![ParsedOp::simple("X", "x"), ParsedOp::simple("Y", "y")];
        let seq = OpSequence::build(ops, &[2, 1], SequencerType::Concat);
        assert_eq!(names(&seq, 6), vec!["X", "X", "Y", "X", "X", "Y"]);
    }

    // --- from_ops tests ---

    #[test]
    fn from_ops_extracts_ratios() {
        let mut op1 = ParsedOp::simple("read", "SELECT");
        op1.params.insert("ratio".into(), serde_json::json!(5));
        let op2 = ParsedOp::simple("write", "INSERT"); // default ratio 1

        let seq = OpSequence::from_ops(vec![op1, op2], SequencerType::Bucket);
        assert_eq!(seq.stanza_length(), 6); // 5 + 1
    }

    // --- General tests ---

    #[test]
    fn sequencer_type_parse() {
        assert_eq!(
            SequencerType::parse("bucket").unwrap(),
            SequencerType::Bucket
        );
        assert_eq!(
            SequencerType::parse("INTERVAL").unwrap(),
            SequencerType::Interval
        );
        assert_eq!(
            SequencerType::parse("Concat").unwrap(),
            SequencerType::Concat
        );
        assert!(SequencerType::parse("bogus").is_err());
    }

    #[test]
    fn single_op_any_strategy() {
        for strategy in [
            SequencerType::Bucket,
            SequencerType::Interval,
            SequencerType::Concat,
        ] {
            let ops = vec![ParsedOp::simple("only", "SELECT 1")];
            let seq = OpSequence::build(ops, &[1], strategy);
            assert_eq!(seq.get(0).name, "only");
            assert_eq!(seq.get(999).name, "only");
        }
    }

    #[test]
    fn stanza_repeats_cleanly() {
        let ops = vec![ParsedOp::simple("A", "a"), ParsedOp::simple("B", "b")];
        let seq = OpSequence::build(ops, &[2, 1], SequencerType::Bucket);
        let stanza = seq.stanza_length();
        // Two full stanzas should produce the same pattern
        let first: Vec<String> = (0..stanza as u64)
            .map(|c| seq.get(c).name.clone())
            .collect();
        let second: Vec<String> = (stanza as u64..2 * stanza as u64)
            .map(|c| seq.get(c).name.clone())
            .collect();
        assert_eq!(first, second);
    }
}
