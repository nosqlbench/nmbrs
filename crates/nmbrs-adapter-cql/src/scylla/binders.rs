// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Type-aware Value→wire-bytes serialization for the scylla adapter.
//!
//! Each prepared-statement parameter has a known [`ColumnType`]
//! retrieved from the prepared-statement metadata. The cycle-time
//! resolved [`Value`](polydat::ast::Value) is wrapped into a
//! lightweight [`NmbrsCell`] that implements [`SerializeValue`].
//! Scylla's blanket `impl<T: SerializeValue> SerializeRow for Vec<T>`
//! then turns a `Vec<NmbrsCell>` into a row that
//! `execute_unpaged` / `batch` consume natively.
//!
//! **Vector binding (SRD 53 §"Native Vector Binding")**: when the
//! column is `vector<float, N>` and the value is
//! [`Value::VecF32`], the [`NmbrsCell::F32Slice`] variant borrows
//! the `&[f32]` directly. Scylla's
//! `impl<T: SerializeValue> SerializeValue for [T]` writes the
//! wire bytes from the slice with no intermediate
//! `CqlValue::Vector(Vec<CqlValue::Float>)` wrapper allocation.
//! Same path for `vector<int, N>` + [`Value::VecI32`].

use polydat::ast::Value;
use scylla::cluster::metadata::{ColumnType, NativeType};
use scylla::serialize::SerializationError;
use scylla::serialize::value::SerializeValue;
use scylla::serialize::writers::{CellWriter, WrittenCellProof};
use scylla::value::CqlValue;

/// Wire-side cell value. Either a [`CqlValue`] (built once for
/// primitives) or a borrowed slice for the typed-vector fast path.
pub(super) enum NmbrsCell<'a> {
    Cql(CqlValue),
    F32Slice(&'a [f32]),
    I32Slice(&'a [i32]),
}

impl<'a> SerializeValue for NmbrsCell<'a> {
    fn serialize<'b>(
        &self,
        typ: &ColumnType,
        writer: CellWriter<'b>,
    ) -> Result<WrittenCellProof<'b>, SerializationError> {
        match self {
            NmbrsCell::Cql(c) => <CqlValue as SerializeValue>::serialize(c, typ, writer),
            NmbrsCell::F32Slice(s) => <[f32] as SerializeValue>::serialize(*s, typ, writer),
            NmbrsCell::I32Slice(s) => <[i32] as SerializeValue>::serialize(*s, typ, writer),
        }
    }
}

/// Build a `Vec<NmbrsCell>` aligned with the prepared statement's
/// variable column specs. Used by both the prepared dispenser
/// (single row) and the batch dispenser (per row).
pub(super) fn build_row<'v>(
    col_specs: scylla::response::query_result::ColumnSpecs<'_, '_>,
    values: &'v [Value],
) -> Result<Vec<NmbrsCell<'v>>, String> {
    let specs = col_specs.as_slice();
    if specs.len() != values.len() {
        return Err(format!(
            "bind arity mismatch: prepared expects {} parameters, got {} values",
            specs.len(),
            values.len(),
        ));
    }
    let mut row: Vec<NmbrsCell<'v>> = Vec::with_capacity(specs.len());
    for (idx, (spec, value)) in specs.iter().zip(values.iter()).enumerate() {
        row.push(value_to_cell(spec.typ(), value).map_err(|e| format!("position {idx}: {e}"))?);
    }
    Ok(row)
}

/// Map one [`Value`] to an [`NmbrsCell`], preferring the borrowed
/// slice path for typed-vector columns.
fn value_to_cell<'v>(col_type: &ColumnType<'_>, value: &'v Value) -> Result<NmbrsCell<'v>, String> {
    use ColumnType as CT;
    match col_type {
        CT::Vector { typ: inner, .. } => match (inner.as_ref(), value) {
            (CT::Native(NativeType::Float), Value::VecF32(arc)) => Ok(NmbrsCell::F32Slice(arc)),
            (CT::Native(NativeType::Int), Value::VecI32(arc)) => Ok(NmbrsCell::I32Slice(arc)),
            // Fallback: build a CqlValue::Vector from non-typed
            // input shapes (Bytes, Str). Kept for migration and
            // for workloads that compute vectors via expression
            // rather than dataset accessors.
            (CT::Native(NativeType::Float), other) => {
                Ok(NmbrsCell::Cql(vector_float_from_value(other)?))
            }
            (other, _) => Err(format!(
                "vector<{other:?}, _> binding from {value:?} not supported"
            )),
        },
        // Native scalars: build a CqlValue once.
        CT::Native(native) => Ok(NmbrsCell::Cql(native_to_cql(native, value)?)),
        // Less-specialized types (collections, UDTs, tuples)
        // fall back to text rendering of the value. Workloads
        // that exercise them can extend the dispatch as needed.
        _ => Ok(NmbrsCell::Cql(CqlValue::Text(value.to_display_string()))),
    }
}

fn native_to_cql(native: &NativeType, value: &Value) -> Result<CqlValue, String> {
    use NativeType as NT;
    match native {
        NT::TinyInt => Ok(CqlValue::TinyInt(value.as_u64() as i8)),
        NT::SmallInt => Ok(CqlValue::SmallInt(value.as_u64() as i16)),
        NT::Int => Ok(CqlValue::Int(value.as_u64() as i32)),
        NT::BigInt => Ok(CqlValue::BigInt(value.as_u64() as i64)),
        NT::Counter => Ok(CqlValue::Counter(scylla::value::Counter(
            value.as_u64() as i64
        ))),
        NT::Float => Ok(CqlValue::Float(value.as_f64() as f32)),
        NT::Double => Ok(CqlValue::Double(value.as_f64())),
        NT::Boolean => Ok(CqlValue::Boolean(value.as_u64() != 0)),
        NT::Text | NT::Ascii => Ok(CqlValue::Text(value.to_display_string())),
        NT::Blob => match value {
            Value::Bytes(b) => Ok(CqlValue::Blob(b.to_vec())),
            other => Err(format!("blob bind: expected Bytes, got {other:?}")),
        },
        NT::Uuid => {
            let s = value.to_display_string();
            uuid::Uuid::parse_str(&s)
                .map(CqlValue::Uuid)
                .map_err(|e| format!("uuid parse '{s}': {e}"))
        }
        NT::Timeuuid => {
            let s = value.to_display_string();
            uuid::Uuid::parse_str(&s)
                .map(|u| CqlValue::Timeuuid(scylla::value::CqlTimeuuid::from(u)))
                .map_err(|e| format!("timeuuid parse '{s}': {e}"))
        }
        // Less-common natives: pass through as text.
        _ => Ok(CqlValue::Text(value.to_display_string())),
    }
}

/// Build a `CqlValue::Vector` of floats from non-native input
/// shapes (`Value::Bytes` raw bytes, `Value::Str` JSON-array
/// text). Only used by the fallback path — typed `Value::VecF32`
/// is handled directly in `value_to_cell`.
fn vector_float_from_value(value: &Value) -> Result<CqlValue, String> {
    let floats: Vec<f32> = match value {
        Value::Bytes(bytes) => {
            if !bytes.len().is_multiple_of(4) {
                return Err(format!(
                    "vector<float, _>: byte buffer length {} not a multiple of 4",
                    bytes.len()
                ));
            }
            bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect()
        }
        Value::Str(s) => parse_float_array(s)
            .ok_or_else(|| format!("vector<float, _>: cannot parse '{s}' as a float array"))?,
        Value::VecF32(_) => unreachable!("VecF32 handled in value_to_cell"),
        other => {
            return Err(format!(
                "vector<float, _>: expected VecF32 (preferred), Bytes, or Str, got {other:?}"
            ));
        }
    };
    Ok(CqlValue::Vector(
        floats.into_iter().map(CqlValue::Float).collect(),
    ))
}

fn parse_float_array(s: &str) -> Option<Vec<f32>> {
    let trimmed = s.trim().trim_start_matches('[').trim_end_matches(']');
    trimmed
        .split(',')
        .map(|p| p.trim().parse::<f32>().ok())
        .collect()
}
