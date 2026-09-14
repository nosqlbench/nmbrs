// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Type-aware CQL encoded-row size estimator, byte-magnitude parsing,
//! and batch row-count planning — the shared numeric backbone of
//! byte-bounded batching (SRD-103 §6).
//!
//! The batch dispensers accumulate rows until the estimated encoded
//! size would cross a byte budget (`max_batch_size`), then flush. The
//! estimate is derived from the same polydat [`Value`]s the binders
//! consume, approximating Cassandra's mutation-size accounting:
//!
//! - fixed-width scalars bind to their CQL wire widths
//!   (`bool` = 1, 32-bit = 4, 64-bit = 8, 128-bit / uuid = 16),
//! - variable-length values (`text`, `blob`, JSON) carry a 4-byte
//!   length prefix plus their payload byte length,
//! - **typed vector slices dominate**: an `f32`/`i32` slice is
//!   `n × 4` bytes, an `f64`/`i64` slice `n × 8`, etc. — the largest
//!   term by far for embedding-insert workloads.
//!
//! The estimate is intentionally conservative (never under-counts a
//! plausible CQL mapping) so the byte budget can be held safely below
//! the server's `batch_size_fail_threshold` without approximation
//! error crossing the reject line (SRD-103 §8).

use polydat::ast::Value;

/// Length prefix CQL writes before a variable-length cell (`[int]`
/// length header) on the wire, in bytes.
const VAR_PREFIX: u64 = 4;

/// Conservative payload estimate for a reflected/adapter-contributed
/// value with no precise width mapping (uuid, inet, timestamp, …).
const UNKNOWN_SCALAR: u64 = 16;

/// Hard cap on the row count predicted for a byte-budgeted op that
/// sets `max_batch_size` without an explicit `batch: N` row cap —
/// the SRD-22 "falls back to 1000-row cap" bound. Prevents a runaway
/// pull when rows are tiny relative to the budget.
pub const MAX_PREDICTED_ROWS: usize = 1000;

/// Approximate the CQL-encoded size of a single bound [`Value`], in
/// bytes. Handles every variant the CQL binders (both engines) map;
/// unknown/reflected variants fall back to a small conservative
/// constant rather than panicking.
pub fn estimate_value_size(v: &Value) -> u64 {
    match v {
        Value::Bool(_) => 1,
        // 64-bit integers most naturally bind to CQL `bigint` /
        // `timestamp` (8 bytes). Use the wider mapping so the
        // estimate never under-counts when the column is 64-bit;
        // a narrower `int` column merely over-estimates by 4.
        Value::U64(_) | Value::I64(_) => 8,
        Value::F64(_) => 8,                                        // double
        Value::U128(_) | Value::I128(_) | Value::Reg128(..) => 16, // uuid / 128-bit
        // Variable-length: 4-byte length prefix + payload bytes.
        Value::Str(s) => VAR_PREFIX + s.len() as u64,
        Value::Bytes(b) => VAR_PREFIX + b.len() as u64,
        Value::Json(j) => VAR_PREFIX + json_estimate(j),
        // Typed vector slices — the dominant term for vector
        // workloads. `n × elem-width` plus the length prefix.
        Value::VecF32(s) => VAR_PREFIX + (s.len() as u64) * 4,
        Value::VecI32(s) => VAR_PREFIX + (s.len() as u64) * 4,
        Value::VecF64(s) => VAR_PREFIX + (s.len() as u64) * 8,
        Value::VecI64(s) => VAR_PREFIX + (s.len() as u64) * 8,
        Value::VecF16(s) => VAR_PREFIX + (s.len() as u64) * 2,
        Value::VecI16(s) => VAR_PREFIX + (s.len() as u64) * 2,
        Value::VecI8(s) => VAR_PREFIX + (s.len() as u64),
        // Reflected protocol values (uuid, inet, timestamp, …) and
        // type-erased resource handles (never bound as a column, but
        // handled for exhaustiveness): a small conservative constant,
        // never panic.
        Value::Ext(_) | Value::Handle(_) => VAR_PREFIX + UNKNOWN_SCALAR,
        // Unset sentinel — contributes no wire bytes (never appears
        // in a real bound row).
        Value::None => 0,
    }
}

/// Per-mutation overhead the server's batch-size accounting adds on
/// top of the bound values: partition-key framing, the table
/// identifier (mutation size is measured per table reference, which
/// is why a longer table name grows every row's accounted size), row
/// liveness/timestamp, and per-cell headers. Measured live against
/// Cassandra 5.0.x this runs ~40–60 bytes per row plus the table
/// name; 128 covers that with headroom for long names. Without this
/// term the estimator under-counted by ~8–10% — exactly the ×0.9
/// back-off — and a 7-character-longer table name once pushed a
/// packed batch 16 bytes over `batch_size_fail_threshold`.
pub const ROW_OVERHEAD: u64 = 128;

/// Estimated server-accounted size of one bound row: the encoded
/// sizes of every bound value plus [`ROW_OVERHEAD`] for the
/// per-mutation framing the server counts and the values don't show.
pub fn estimate_row_size(values: &[Value]) -> u64 {
    ROW_OVERHEAD + values.iter().map(estimate_value_size).sum::<u64>()
}

/// Cheap structural size estimate for a JSON value, avoiding a full
/// re-serialization on the batch path. Approximates the rendered
/// text length.
fn json_estimate(j: &serde_json::Value) -> u64 {
    match j {
        serde_json::Value::Null => 4,
        serde_json::Value::Bool(_) => 5,
        serde_json::Value::Number(_) => 8,
        serde_json::Value::String(s) => s.len() as u64 + 2,
        serde_json::Value::Array(a) => {
            2 + a.len() as u64 + a.iter().map(json_estimate).sum::<u64>()
        }
        serde_json::Value::Object(o) => {
            2 + o
                .iter()
                .map(|(k, v)| k.len() as u64 + 4 + json_estimate(v))
                .sum::<u64>()
        }
    }
}

/// Parse a `max_batch_size` op-field value into a byte magnitude.
///
/// Accepts a bare JSON number (`65536`) or a magnitude string
/// (`"64KB"`, `"128KiB"`, `"1MB"`, `"64k"`). Returns `None` for a
/// missing field or an unparseable value (→ no byte cap).
pub fn parse_max_batch_bytes(param: Option<&serde_json::Value>) -> Option<u64> {
    let v = param?;
    if let Some(u) = v.as_u64() {
        return Some(u);
    }
    if let Some(f) = v.as_f64()
        && f.is_finite()
        && f >= 0.0
    {
        return Some(f.round() as u64);
    }
    if let Some(s) = v.as_str() {
        return parse_byte_magnitude(s);
    }
    None
}

/// Parse a byte-magnitude string.
///
/// Delegates to [`nmbrs_workload::magnitude::parse_magnitude`], which
/// understands single-letter decimal (`k m g t p e`, powers of 1000)
/// and two-letter binary (`ki mi gi …`, powers of 1024) suffixes.
/// That parser does **not** recognise the trailing byte-unit marker
/// used throughout the workloads (`64KB`, `128KiB`, `1MB` all return
/// `None`), so a single trailing `b`/`B` is stripped as a "bytes"
/// marker before a retry:
///
/// - `"64KB"`  → `64K`  → 64_000   (decimal `KB` = 1000)
/// - `"64KiB"` → `64Ki` → 65_536   (binary `KiB` = 1024)
/// - `"128KB"` → 128_000
/// - `"1MB"`   → 1_000_000
/// - `"1024"`  → 1_024   (bare number — no marker to strip)
///
/// Magnitude-native spellings that the base parser already accepts
/// (`64k`, `64ki`, scientific `6.4e4`, and the bare-`b` = billion
/// decimal alias) keep their meaning, because the raw string is tried
/// first and only a `None` result triggers the byte-marker retry.
pub fn parse_byte_magnitude(raw: &str) -> Option<u64> {
    let t = raw.trim();
    let parsed = nmbrs_workload::magnitude::parse_magnitude(t).or_else(|| {
        let stripped = t.strip_suffix(['b', 'B'])?;
        nmbrs_workload::magnitude::parse_magnitude(stripped)
    })?;
    if parsed.is_finite() && parsed >= 0.0 {
        Some(parsed.round() as u64)
    } else {
        None
    }
}

/// Settle the FIXED, uniform batch stride `N` — the number of cursor
/// ordinals one batch-op invocation reads AND advances — from the
/// characterized row size and the two caps. Computed ONCE at `map_op`
/// (not per-execute), so `OpDispenser::rows_per_op` can report it and
/// the executor drives the phase cursor with `Σ rows_per_op` (SRD-22
/// cover-once: fetch N, advance N — no over-insert).
///
/// - `batch: N` (with OR without `max_batch_size`) → exactly `N`, used
///   **directly and unfloored**. The `batch:` field is workload-authored
///   (GK-resolved upstream), so the workload owns the cursor stride; the
///   adapter never caps it against the byte budget. When a `max_batch_size`
///   byte budget is also set it governs only the dynamic byte-fill in
///   `execute`, not this fixed stride.
/// - `max_batch_size` alone (no `batch:`) → the UNFLOORED reserve
///   `(budget / row_size)` clamped to `1..=`[`MAX_PREDICTED_ROWS`]. This
///   sizes the reserve to one TRUE budget-worth of rows; the dynamic
///   byte-fill in `execute` then fills to the real byte budget, so the
///   op's `element_count` reports the ACTUAL number of rows batched — not a
///   power-of-ten approximation of it. Any round-number shaping of that
///   measurement is left entirely to the workload (e.g. a
///   `floor_decade(count)` result-binding): the adapter measures the
///   truth, the workload decides how to round it.
/// - neither → a single row (unchanged single-op behavior).
///
/// The `budget / row_size` reserve is always `>= 1` (a single oversized
/// row still reserves one) and never exceeds [`MAX_PREDICTED_ROWS`], the
/// SRD-22 runaway-pull guard.
pub fn fixed_batch_stride(row_size: u64, batch_n: usize, budget: Option<u64>) -> usize {
    let budget_rows = |b: u64| -> usize {
        // Unfloored: how many characterized rows fit in one budget-worth.
        // Integer division truncates DOWN to the last whole row that fits,
        // so the reserve never over-commits past the byte budget.
        let rows = (b / row_size.max(1)).max(1);
        (rows as usize).clamp(1, MAX_PREDICTED_ROWS)
    };
    match (batch_n, budget) {
        // A workload-authored `batch:` count is the stride, verbatim and
        // unfloored — never capped against the byte budget.
        (n, _) if n > 0 => n,
        // `max_batch_size` alone → one budget-worth, UNFLOORED (the byte-fill
        // fills to the true budget; the workload owns any rounding).
        (0, Some(b)) => budget_rows(b),
        _ => 1,
    }
}

/// Characterize a representative row's estimated CQL-encoded size by
/// evaluating the op's bound-value wires at cursor offset `0` through a
/// fresh instance of the phase-scope (`parent`) kernel. Called ONCE at
/// `map_op` to settle the fixed batch stride (see [`fixed_batch_stride`]).
///
/// The probe kernel is built with the SAME primitive a fiber uses per
/// cycle ([`PolydatKernel::for_iteration`], wired under `parent`), then
/// read through the SAME [`CycleWires`] surface the batch dispenser uses
/// at execute time — so the sampled row-0 values (the dominant dataset
/// vector included) match what the op will actually bind. `parent` is
/// borrowed immutably and unaffected: pulling outputs evaluates the DAG
/// without committing any write-through, so no shared cell is mutated.
pub fn characterize_row_size(
    parent: &std::sync::Arc<polydat::kernel::PolydatKernel>,
    bind_names: &[String],
) -> u64 {
    use nmbrs_runtime::wires::{CycleWires, WireSource};
    // `for_iteration` returns a fresh, uniquely-owned Arc (refcount 1),
    // so `try_unwrap` yields the owned, mutable kernel we position at 0.
    let probe = polydat::kernel::PolydatKernel::for_iteration(parent, parent, &[]);
    let mut probe = std::sync::Arc::try_unwrap(probe)
        .ok()
        .expect("for_iteration returns a uniquely-owned kernel");
    let wires = CycleWires::new(&mut probe);
    wires.advance(0);
    let values: Vec<Value> = bind_names
        .iter()
        .map(|n| wires.get(n).unwrap_or(Value::None))
        .collect();
    estimate_row_size(&values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use polydat::ast::{SliceArc, Value};

    fn vecf32(n: usize) -> Value {
        Value::VecF32(SliceArc::from_vec(vec![0.0f32; n]))
    }
    fn veci32(n: usize) -> Value {
        Value::VecI32(SliceArc::from_vec(vec![0i32; n]))
    }

    #[test]
    fn f32_vector_row_is_payload_plus_small_prefix() {
        // A 1536-dim f32 embedding dominates the row: n*4 bytes plus
        // the 4-byte length prefix.
        let row = [vecf32(1536)];
        let est = estimate_row_size(&row);
        assert_eq!(est, ROW_OVERHEAD + VAR_PREFIX + 1536 * 4);
        // "≈ 1536*4": prefix + per-mutation overhead are small
        // constants next to the payload.
        assert!(
            est >= 1536 * 4 && est <= 1536 * 4 + ROW_OVERHEAD + 8,
            "est={est}"
        );
    }

    #[test]
    fn i32_vector_row_matches_f32_width() {
        assert_eq!(
            estimate_row_size(&[veci32(1536)]),
            ROW_OVERHEAD + VAR_PREFIX + 1536 * 4
        );
    }

    #[test]
    fn wide_and_half_vectors() {
        // f64/i64 = n*8, f16/i16 = n*2, i8 = n*1.
        assert_eq!(
            estimate_row_size(&[Value::VecF64(SliceArc::from_vec(vec![0.0f64; 4]))]),
            ROW_OVERHEAD + VAR_PREFIX + 4 * 8
        );
        assert_eq!(
            estimate_row_size(&[Value::VecI16(SliceArc::from_vec(vec![0i16; 8]))]),
            ROW_OVERHEAD + VAR_PREFIX + 8 * 2
        );
        assert_eq!(
            estimate_row_size(&[Value::VecI8(SliceArc::from_vec(vec![0i8; 10]))]),
            ROW_OVERHEAD + VAR_PREFIX + 10
        );
    }

    #[test]
    fn mixed_scalar_row() {
        // id(bigint=8) + flag(bool=1) + name(text: 4+3) + vec(4+16).
        let row = [
            Value::U64(42),
            Value::Bool(true),
            Value::Str("abc".into()),
            veci32(4),
        ];
        let expected = ROW_OVERHEAD + 8 + 1 + (VAR_PREFIX + 3) + (VAR_PREFIX + 16);
        assert_eq!(estimate_row_size(&row), expected);
    }

    #[test]
    fn fixed_width_scalars() {
        assert_eq!(estimate_value_size(&Value::Bool(false)), 1);
        assert_eq!(estimate_value_size(&Value::U64(1)), 8);
        assert_eq!(estimate_value_size(&Value::I64(-1)), 8);
        assert_eq!(estimate_value_size(&Value::F64(1.5)), 8);
        assert_eq!(
            estimate_value_size(&Value::Bytes([0u8; 12].into())),
            VAR_PREFIX + 12
        );
        assert_eq!(estimate_value_size(&Value::None), 0);
    }

    #[test]
    fn byte_magnitude_accepts_workload_spellings() {
        // parse_magnitude alone rejects these (returns None); the
        // trailing-byte-marker retry rescues them.
        assert_eq!(parse_byte_magnitude("64KB"), Some(64_000));
        assert_eq!(parse_byte_magnitude("128KB"), Some(128_000));
        assert_eq!(parse_byte_magnitude("1MB"), Some(1_000_000));
        assert_eq!(parse_byte_magnitude("64KiB"), Some(65_536));
        assert_eq!(parse_byte_magnitude("50KiB"), Some(51_200));
        // Bare numbers and magnitude-native spellings pass through.
        assert_eq!(parse_byte_magnitude("1024"), Some(1024));
        assert_eq!(parse_byte_magnitude("64k"), Some(64_000));
        assert_eq!(parse_byte_magnitude("64Ki"), Some(65_536));
        // Junk → None (no byte cap).
        assert_eq!(parse_byte_magnitude("lots"), None);
    }

    #[test]
    fn parse_max_batch_bytes_from_json() {
        assert_eq!(
            parse_max_batch_bytes(Some(&serde_json::json!("64KB"))),
            Some(64_000)
        );
        assert_eq!(
            parse_max_batch_bytes(Some(&serde_json::json!(65536))),
            Some(65_536)
        );
        assert_eq!(parse_max_batch_bytes(None), None);
    }

    #[test]
    fn fixed_batch_stride_matrix() {
        // `batch: N` alone → exactly N, never floored.
        assert_eq!(fixed_batch_stride(6148, 8, None), 8);
        assert_eq!(fixed_batch_stride(6148, 300, None), 300);
        // `max_batch_size` alone → one budget-worth, UNFLOORED (truncated
        // integer division). 64_000 / 6_148 = 10.4 → 10 rows fit.
        assert_eq!(fixed_batch_stride(6148, 0, Some(64_000)), 10);
        // Unfloored vs floored contrast: 64_000 / 1_000 = 64 whole rows.
        // The old floor_base10 path would have collapsed this to 10; the
        // measure path now reserves the true count so element_count is real.
        assert_eq!(fixed_batch_stride(1000, 0, Some(64_000)), 64);
        // Another non-power-of-ten reserve survives unrounded: 90_000 /
        // 300 = 300 rows (floor_base10 would have been 100).
        assert_eq!(fixed_batch_stride(300, 0, Some(90_000)), 300);
        // both → `batch_n` used directly, unfloored: the workload authors the
        // stride and the byte budget no longer caps it (it drives only the
        // dynamic byte-fill in execute). 128_000/6_148=20.8 would floor to 10,
        // but batch: 200 wins verbatim.
        assert_eq!(fixed_batch_stride(6148, 200, Some(128_000)), 200);
        // both, generous budget → still `batch_n` verbatim.
        assert_eq!(fixed_batch_stride(6148, 200, Some(10_000_000)), 200);
        // tiny rows: 10_000_000 / 1 rows clamps to the 1000-row cap.
        assert_eq!(
            fixed_batch_stride(1, 0, Some(10_000_000)),
            MAX_PREDICTED_ROWS
        );
        // oversized single row (row > budget) still yields ≥1.
        assert_eq!(fixed_batch_stride(100_000, 0, Some(64_000)), 1);
        // neither → single row.
        assert_eq!(fixed_batch_stride(6148, 0, None), 1);
    }

    #[test]
    fn characterize_row_size_evaluates_wires_at_coord_zero() {
        use polydat::dsl::compile::compile_polydat;
        // A coord-driven output: at cursor offset 0, `val` = 0 * 8 = 0.
        // The probe must set the coord to 0 and pull `val` through the
        // same wire surface the batch dispenser uses at execute time.
        let kernel =
            compile_polydat("input cycle: u64\nval := cycle * 8\n").expect("compile probe program");
        let parent = std::sync::Arc::new(kernel);
        // One bind name → a single U64 → 8 bytes (bigint wire width)
        // plus the per-mutation overhead every row carries.
        assert_eq!(
            characterize_row_size(&parent, &["val".to_string()]),
            ROW_OVERHEAD + 8
        );
        // An undeclared wire resolves to None → contributes 0 value
        // bytes (overhead only), never panics.
        assert_eq!(
            characterize_row_size(&parent, &["nope".to_string()]),
            ROW_OVERHEAD
        );
    }
}
