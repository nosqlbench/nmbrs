// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Native scylla `QueryResult` body for nmbrs validation /
//! capture. Mirrors the cassandra-cpp adapter's `CqlResultBody`
//! shape so downstream wrappers see the same row/column surface
//! regardless of which engine produced the result.

use nmbrs_runtime::adapter::ResultBody;
use scylla::response::query_result::QueryResult;
use scylla::value::{CqlValue, Row};
use std::any::Any;
use std::collections::HashMap;

/// Engine-agnostic result body. Holds resolved row data as
/// rows × column-name maps. Each cell is converted to
/// `serde_json::Value` for uniform downstream handling — the
/// trait surface is `to_json` + `as_any`, just like every other
/// `ResultBody` implementer.
#[derive(Debug)]
pub(super) struct ScyllaResultBody {
    /// Returned row data: each row is a column-name → JSON value map.
    /// Populated for a result that carries a row-set (a SELECT, or an
    /// LWT's `[applied]` acknowledgment); empty for a plain write.
    pub rows: Vec<HashMap<String, serde_json::Value>>,
    /// Rows WRITTEN by this op. A CQL write (INSERT/UPDATE/DELETE, a
    /// whole BATCH, or a DDL ack) returns no row-set, so its result
    /// carries the count of rows it wrote here instead. `element_count`
    /// reports this when there are no returned rows — so a write's
    /// `count` reflects the rows it wrote exactly as a read's reflects
    /// the rows it returned (the two are mutually exclusive: a write
    /// carries no returned rows). `0` for a read.
    written_rows: u64,
}

impl ScyllaResultBody {
    pub fn from_query_result(result: QueryResult) -> Self {
        // Classify by whether the server sent a result-set schema:
        // `into_rows_result()` is Ok for a rows result (a SELECT/LWT,
        // possibly zero rows) and Err for a write/DDL acknowledgment
        // (no row-set). A read carries its returned rows and writes 0;
        // a single write/DDL statement acknowledges one written row.
        match result.into_rows_result() {
            Ok(rows_result) => {
                let cols: Vec<String> = rows_result
                    .column_specs()
                    .iter()
                    .map(|spec| spec.name().to_string())
                    .collect();
                let mut row_maps: Vec<HashMap<String, serde_json::Value>> = Vec::new();
                if let Ok(iter) = rows_result.rows::<Row>() {
                    for row_result in iter {
                        let Ok(row) = row_result else { continue };
                        let mut row_map = HashMap::new();
                        for (idx, cell) in row.columns.iter().enumerate() {
                            let name = cols.get(idx).cloned().unwrap_or_default();
                            row_map.insert(name, cql_to_json(cell.as_ref()));
                        }
                        row_maps.push(row_map);
                    }
                }
                Self {
                    rows: row_maps,
                    written_rows: 0,
                }
            }
            Err(_) => Self {
                rows: Vec::new(),
                written_rows: 1,
            },
        }
    }

    /// A write result carrying an explicit rows-written count. Used
    /// where the dispenser knows the count directly — a BATCH of `n`
    /// rows. No returned rows, so `element_count()` reports `n`.
    pub fn write_ack(rows_written: u64) -> Self {
        Self {
            rows: Vec::new(),
            written_rows: rows_written,
        }
    }
}

impl ResultBody for ScyllaResultBody {
    fn to_json(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.rows
                .iter()
                .map(|row| serde_json::Value::Object(row.clone().into_iter().collect()))
                .collect(),
        )
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn element_count(&self) -> u64 {
        // A read reports rows returned; a write reports rows written.
        // The two are mutually exclusive — a write carries no returned
        // rows — so returned rows win when present and the written
        // count carries the write case.
        if self.rows.is_empty() {
            self.written_rows
        } else {
            self.rows.len() as u64
        }
    }
}

/// Convert a CqlValue cell to `serde_json::Value`. Same projection
/// rules the cassandra-cpp adapter uses, so downstream validation
/// / captures see the same shape regardless of engine.
fn cql_to_json(value: Option<&CqlValue>) -> serde_json::Value {
    use serde_json::Value as J;
    let Some(v) = value else {
        return J::Null;
    };
    match v {
        CqlValue::Boolean(b) => J::Bool(*b),
        CqlValue::TinyInt(n) => J::from(*n),
        CqlValue::SmallInt(n) => J::from(*n),
        CqlValue::Int(n) => J::from(*n),
        CqlValue::BigInt(n) => J::from(*n),
        CqlValue::Counter(c) => J::from(c.0),
        CqlValue::Float(f) => serde_json::Number::from_f64(*f as f64)
            .map(J::Number)
            .unwrap_or(J::Null),
        CqlValue::Double(f) => serde_json::Number::from_f64(*f)
            .map(J::Number)
            .unwrap_or(J::Null),
        CqlValue::Text(s) | CqlValue::Ascii(s) => J::String(s.clone()),
        CqlValue::Uuid(u) => J::String(u.to_string()),
        CqlValue::Timeuuid(u) => J::String(u.to_string()),
        CqlValue::Inet(ip) => J::String(ip.to_string()),
        CqlValue::Blob(b) => J::String(hex_encode(b)),
        CqlValue::List(items) | CqlValue::Set(items) | CqlValue::Vector(items) => {
            J::Array(items.iter().map(|v| cql_to_json(Some(v))).collect())
        }
        CqlValue::Map(entries) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in entries {
                let key = match k {
                    CqlValue::Text(s) | CqlValue::Ascii(s) => s.clone(),
                    other => format!("{other:?}"),
                };
                obj.insert(key, cql_to_json(Some(v)));
            }
            J::Object(obj)
        }
        CqlValue::Tuple(items) => {
            J::Array(items.iter().map(|opt| cql_to_json(opt.as_ref())).collect())
        }
        CqlValue::Empty => J::Null,
        other => J::String(format!("{other:?}")),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_result_reports_rows_written_as_element_count() {
        // A CQL write carries no returned rows; its element_count is
        // the number of rows it wrote — a single write = 1, a batch = n.
        assert_eq!(ScyllaResultBody::write_ack(1).element_count(), 1);
        assert_eq!(ScyllaResultBody::write_ack(500).element_count(), 500);
        // The `written_rows` count is not shadowed by an empty row-set
        // — a write body still serializes to an empty row array (its
        // `count`, not its `body`, carries the write result).
        assert_eq!(
            ScyllaResultBody::write_ack(7).to_json(),
            serde_json::Value::Array(Vec::new()),
        );
    }

    #[test]
    fn read_result_reports_returned_rows_as_element_count() {
        // A read (returned rows present) reports the returned-row count
        // and ignores any written count.
        let mut row = HashMap::new();
        row.insert("key".to_string(), serde_json::json!("v"));
        let body = ScyllaResultBody {
            rows: vec![row],
            written_rows: 0,
        };
        assert_eq!(body.element_count(), 1);
    }

    #[test]
    fn empty_read_reports_zero_not_a_write() {
        // A SELECT that matched no rows is still a read: no returned
        // rows AND no written count, so element_count is 0 (preserving
        // the pre-change no-body behavior for empty reads).
        let body = ScyllaResultBody {
            rows: Vec::new(),
            written_rows: 0,
        };
        assert_eq!(body.element_count(), 0);
    }
}
