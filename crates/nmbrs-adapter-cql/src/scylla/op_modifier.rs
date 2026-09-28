// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Scylla-engine per-statement modifier impls for the universal
//! CQL field surface (SRD 73).
//!
//! Each modifier captures the resolved value at dispenser-
//! initializer time and applies it via the engine's per-statement
//! setter at execute time. A **batch is a statement too**: scylla's
//! `Statement` (unprepared), `PreparedStatement`, AND `Batch` share the
//! same aspect setters (bar `set_page_size`, which is meaningless for a
//! batch of writes), so the marker trait [`ScyllaStmt`] abstracts all
//! three and one modifier chain governs every executable op uniformly.
//! `page_size` on a `Batch` is an intentional no-op — the only aspect that
//! does not apply to a batch — so no aspect can silently skip a batch.

use std::sync::Arc;
use std::time::Duration;

use nmbrs_runtime::op_modifier::OpFieldModifier;
use polydat::ast::Value;
use scylla::statement::batch::Batch;
use scylla::statement::prepared::PreparedStatement;
use scylla::statement::{Consistency, SerialConsistency, Statement};

use crate::common::CqlConsistency;
use crate::common::op_modifier::{CqlModifierFactory, parse_consistency};

/// Setter surface shared by scylla's `Statement` and
/// `PreparedStatement`. Universal CQL fields target one of
/// these methods; modifiers are generic over `S: ScyllaStmt`.
pub trait ScyllaStmt: Send + Sync + 'static {
    fn set_consistency(&mut self, c: Consistency);
    fn set_serial_consistency(&mut self, sc: Option<SerialConsistency>);
    fn set_request_timeout(&mut self, timeout: Option<Duration>);
    fn set_page_size(&mut self, page_size: i32);
    fn set_tracing(&mut self, b: bool);
}

impl ScyllaStmt for Statement {
    fn set_consistency(&mut self, c: Consistency) {
        Statement::set_consistency(self, c);
    }
    fn set_serial_consistency(&mut self, sc: Option<SerialConsistency>) {
        Statement::set_serial_consistency(self, sc);
    }
    fn set_request_timeout(&mut self, t: Option<Duration>) {
        Statement::set_request_timeout(self, t);
    }
    fn set_page_size(&mut self, n: i32) {
        Statement::set_page_size(self, n);
    }
    fn set_tracing(&mut self, b: bool) {
        Statement::set_tracing(self, b);
    }
}

impl ScyllaStmt for PreparedStatement {
    fn set_consistency(&mut self, c: Consistency) {
        PreparedStatement::set_consistency(self, c);
    }
    fn set_serial_consistency(&mut self, sc: Option<SerialConsistency>) {
        PreparedStatement::set_serial_consistency(self, sc);
    }
    fn set_request_timeout(&mut self, t: Option<Duration>) {
        PreparedStatement::set_request_timeout(self, t);
    }
    fn set_page_size(&mut self, n: i32) {
        PreparedStatement::set_page_size(self, n);
    }
    fn set_tracing(&mut self, b: bool) {
        PreparedStatement::set_tracing(self, b);
    }
}

impl ScyllaStmt for Batch {
    fn set_consistency(&mut self, c: Consistency) {
        Batch::set_consistency(self, c);
    }
    fn set_serial_consistency(&mut self, sc: Option<SerialConsistency>) {
        Batch::set_serial_consistency(self, sc);
    }
    fn set_request_timeout(&mut self, t: Option<Duration>) {
        Batch::set_request_timeout(self, t);
    }
    // A batch is a set of writes; CQL paging applies to reads, so page size is
    // inert for a batch. The one aspect that does not map — an explicit no-op.
    fn set_page_size(&mut self, _n: i32) {}
    fn set_tracing(&mut self, b: bool) {
        Batch::set_tracing(self, b);
    }
}

// =========================================================================
// Per-field modifier impls
// =========================================================================

struct ConsistencyMod {
    consistency: Consistency,
    display: &'static str,
}

impl<S: ScyllaStmt> OpFieldModifier<S> for ConsistencyMod {
    fn field_name(&self) -> &'static str {
        "consistency"
    }
    fn apply(&self, s: &mut S) {
        s.set_consistency(self.consistency);
    }
    fn diagnostic_value(&self) -> serde_json::Value {
        serde_json::Value::String(self.display.to_string())
    }
}

struct SerialConsistencyMod {
    serial: SerialConsistency,
    display: &'static str,
}

impl<S: ScyllaStmt> OpFieldModifier<S> for SerialConsistencyMod {
    fn field_name(&self) -> &'static str {
        "serial_consistency"
    }
    fn apply(&self, s: &mut S) {
        s.set_serial_consistency(Some(self.serial));
    }
    fn diagnostic_value(&self) -> serde_json::Value {
        serde_json::Value::String(self.display.to_string())
    }
}

struct RequestTimeoutMod {
    timeout: Duration,
}

impl<S: ScyllaStmt> OpFieldModifier<S> for RequestTimeoutMod {
    fn field_name(&self) -> &'static str {
        "request_timeout_ms"
    }
    fn apply(&self, s: &mut S) {
        s.set_request_timeout(Some(self.timeout));
    }
    fn diagnostic_value(&self) -> serde_json::Value {
        serde_json::Value::from(self.timeout.as_millis() as u64)
    }
}

struct PageSizeMod {
    page_size: i32,
}

impl<S: ScyllaStmt> OpFieldModifier<S> for PageSizeMod {
    fn field_name(&self) -> &'static str {
        "page_size"
    }
    fn apply(&self, s: &mut S) {
        s.set_page_size(self.page_size);
    }
    fn diagnostic_value(&self) -> serde_json::Value {
        serde_json::Value::from(self.page_size as i64)
    }
}

struct CqlTraceMod {
    tracing: bool,
}

impl<S: ScyllaStmt> OpFieldModifier<S> for CqlTraceMod {
    fn field_name(&self) -> &'static str {
        "cql_trace"
    }
    fn apply(&self, s: &mut S) {
        s.set_tracing(self.tracing);
    }
    fn diagnostic_value(&self) -> serde_json::Value {
        serde_json::Value::Bool(self.tracing)
    }
}

// =========================================================================
// Factory
// =========================================================================

/// Phantom-typed factory parameterised over the engine-native
/// statement type. Use `ScyllaModifierFactory<Statement>` for
/// raw / unprepared dispatch and
/// `ScyllaModifierFactory<PreparedStatement>` for prepared
/// dispatch.
pub struct ScyllaModifierFactory<S>(std::marker::PhantomData<fn() -> S>);

impl<S: ScyllaStmt> CqlModifierFactory for ScyllaModifierFactory<S> {
    type Statement = S;

    fn modifier_for(
        field: &'static str,
        value: Value,
    ) -> Result<Option<Box<dyn OpFieldModifier<S>>>, String> {
        match field {
            "consistency" => {
                let s = match &value {
                    Value::Str(s) => s.as_ref(),
                    other => return Err(format!("expected str, got {:?}", other.port_type())),
                };
                let cl = parse_consistency(s)?;
                Ok(Some(Box::new(ConsistencyMod {
                    consistency: cql_to_scylla(cl),
                    display: cql_consistency_display(cl),
                })))
            }
            "serial_consistency" => {
                let s = match &value {
                    Value::Str(s) => s.as_ref(),
                    other => return Err(format!("expected str, got {:?}", other.port_type())),
                };
                let (serial, display) = parse_serial_consistency(s)?;
                Ok(Some(Box::new(SerialConsistencyMod { serial, display })))
            }
            "request_timeout_ms" => {
                let ms = match &value {
                    Value::U64(v) => *v,
                    other => return Err(format!("expected u64, got {:?}", other.port_type())),
                };
                Ok(Some(Box::new(RequestTimeoutMod {
                    timeout: Duration::from_millis(ms),
                })))
            }
            "page_size" => {
                let n = match &value {
                    Value::U64(v) => i32::try_from(*v)
                        .map_err(|_| format!("page_size {v} does not fit in i32"))?,
                    other => return Err(format!("expected u64, got {:?}", other.port_type())),
                };
                if n <= 0 {
                    return Err(format!("page_size must be positive, got {n}"));
                }
                Ok(Some(Box::new(PageSizeMod { page_size: n })))
            }
            "cql_trace" => {
                let b = match &value {
                    Value::Bool(v) => *v,
                    other => return Err(format!("expected bool, got {:?}", other.port_type())),
                };
                Ok(Some(Box::new(CqlTraceMod { tracing: b })))
            }
            _ => Err(format!("unknown universal field '{field}'")),
        }
    }
}

// =========================================================================
// Helpers
// =========================================================================

fn cql_to_scylla(c: CqlConsistency) -> Consistency {
    match c {
        CqlConsistency::Any => Consistency::Any,
        CqlConsistency::One => Consistency::One,
        CqlConsistency::Two => Consistency::Two,
        CqlConsistency::Three => Consistency::Three,
        CqlConsistency::Quorum => Consistency::Quorum,
        CqlConsistency::All => Consistency::All,
        CqlConsistency::LocalQuorum => Consistency::LocalQuorum,
        CqlConsistency::EachQuorum => Consistency::EachQuorum,
        CqlConsistency::LocalOne => Consistency::LocalOne,
    }
}

fn cql_consistency_display(c: CqlConsistency) -> &'static str {
    match c {
        CqlConsistency::Any => "ANY",
        CqlConsistency::One => "ONE",
        CqlConsistency::Two => "TWO",
        CqlConsistency::Three => "THREE",
        CqlConsistency::Quorum => "QUORUM",
        CqlConsistency::All => "ALL",
        CqlConsistency::LocalQuorum => "LOCAL_QUORUM",
        CqlConsistency::EachQuorum => "EACH_QUORUM",
        CqlConsistency::LocalOne => "LOCAL_ONE",
    }
}

/// CQL only defines two serial-consistency levels (SRD 73 superset
/// keeps it minimal): `SERIAL` and `LOCAL_SERIAL`. Returns the
/// scylla-typed enum + the canonical display string for the
/// event-log diagnostic.
fn parse_serial_consistency(s: &str) -> Result<(SerialConsistency, &'static str), String> {
    match s.to_uppercase().as_str() {
        "SERIAL" => Ok((SerialConsistency::Serial, "SERIAL")),
        "LOCAL_SERIAL" => Ok((SerialConsistency::LocalSerial, "LOCAL_SERIAL")),
        _ => Err(format!(
            "unrecognized serial_consistency '{s}'. Valid: SERIAL, LOCAL_SERIAL"
        )),
    }
}

// Suppress "unused" warnings for the Arc re-export pulled in by
// callers via this module's path.
#[allow(dead_code)]
fn _arc_anchor() -> Option<Arc<()>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_timeout_modifier_builds() {
        let m: Box<dyn OpFieldModifier<Statement>> =
            ScyllaModifierFactory::<Statement>::modifier_for(
                "request_timeout_ms",
                Value::U64(300_000),
            )
            .unwrap()
            .unwrap();
        assert_eq!(m.field_name(), "request_timeout_ms");
        assert_eq!(m.diagnostic_value(), serde_json::json!(300_000u64));
    }

    #[test]
    fn consistency_modifier_builds_with_display() {
        let m: Box<dyn OpFieldModifier<Statement>> =
            ScyllaModifierFactory::<Statement>::modifier_for(
                "consistency",
                Value::Str(std::sync::Arc::from("LOCAL_QUORUM")),
            )
            .unwrap()
            .unwrap();
        assert_eq!(m.field_name(), "consistency");
        assert_eq!(m.diagnostic_value(), serde_json::json!("LOCAL_QUORUM"));
    }

    fn expect_err<T>(r: Result<T, String>) -> String {
        match r {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(e) => e,
        }
    }

    #[test]
    fn consistency_rejects_unknown_level() {
        let err = expect_err(ScyllaModifierFactory::<Statement>::modifier_for(
            "consistency",
            Value::Str(std::sync::Arc::from("NOT_A_LEVEL")),
        ));
        assert!(err.contains("NOT_A_LEVEL"));
        assert!(err.contains("LOCAL_ONE"), "lists valid names: {err}");
    }

    #[test]
    fn page_size_rejects_zero_and_negative() {
        let err = expect_err(ScyllaModifierFactory::<Statement>::modifier_for(
            "page_size",
            Value::U64(0),
        ));
        assert!(err.contains("positive"), "{err}");
    }

    #[test]
    fn page_size_rejects_overflow() {
        let err = expect_err(ScyllaModifierFactory::<Statement>::modifier_for(
            "page_size",
            Value::U64(u64::MAX),
        ));
        assert!(err.contains("i32"), "{err}");
    }

    #[test]
    fn request_timeout_rejects_non_u64() {
        let err = expect_err(ScyllaModifierFactory::<Statement>::modifier_for(
            "request_timeout_ms",
            Value::Bool(true),
        ));
        assert!(err.contains("expected u64"), "{err}");
    }

    #[test]
    fn cql_trace_accepts_bool() {
        let m: Box<dyn OpFieldModifier<Statement>> =
            ScyllaModifierFactory::<Statement>::modifier_for("cql_trace", Value::Bool(true))
                .unwrap()
                .unwrap();
        assert_eq!(m.diagnostic_value(), serde_json::json!(true));
    }

    #[test]
    fn serial_consistency_parses_known_levels() {
        let m: Box<dyn OpFieldModifier<Statement>> =
            ScyllaModifierFactory::<Statement>::modifier_for(
                "serial_consistency",
                Value::Str(std::sync::Arc::from("LOCAL_SERIAL")),
            )
            .unwrap()
            .unwrap();
        assert_eq!(m.diagnostic_value(), serde_json::json!("LOCAL_SERIAL"));
    }

    #[test]
    fn unknown_field_rejected() {
        let err = expect_err(ScyllaModifierFactory::<Statement>::modifier_for(
            "not_a_field",
            Value::U64(1),
        ));
        assert!(err.contains("unknown universal field"), "{err}");
    }

    /// End-to-end: build chain, apply to a real Statement, verify
    /// the underlying setters fired. We use side-effect probes
    /// (a Statement we mutate, then introspect) — scylla exposes
    /// no public getter for these so we trust the setter signatures
    /// and exercise the boxed-trait machinery.
    #[test]
    fn modifier_apply_compiles_against_statement() {
        let m: Box<dyn OpFieldModifier<Statement>> =
            ScyllaModifierFactory::<Statement>::modifier_for(
                "request_timeout_ms",
                Value::U64(60_000),
            )
            .unwrap()
            .unwrap();
        let mut stmt = Statement::new("SELECT 1".to_string());
        m.apply(&mut stmt); // should not panic
    }

    #[test]
    fn modifier_apply_compiles_against_prepared() {
        // We can't easily construct a PreparedStatement in
        // isolation (requires a session round-trip), but we CAN
        // confirm the trait impls compile and type-check.
        fn _typecheck() {
            let _: Box<dyn OpFieldModifier<PreparedStatement>> = Box::new(RequestTimeoutMod {
                timeout: Duration::from_millis(1),
            });
        }
    }

    /// A batch is a statement too: the SAME factory resolves a
    /// batch-targeted modifier, and it applies to a real `Batch` — so the
    /// request timeout (and every other universal field) reaches the batch
    /// itself, not just its member statements.
    #[test]
    fn modifier_apply_compiles_against_batch() {
        use scylla::statement::batch::{Batch, BatchType};
        let m: Box<dyn OpFieldModifier<Batch>> =
            ScyllaModifierFactory::<Batch>::modifier_for("request_timeout_ms", Value::U64(60_000))
                .unwrap()
                .unwrap();
        let mut batch = Batch::new(BatchType::Unlogged);
        m.apply(&mut batch); // should not panic
    }
}
