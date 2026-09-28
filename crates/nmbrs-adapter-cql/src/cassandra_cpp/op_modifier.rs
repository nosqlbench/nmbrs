// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! cassandra-cpp-engine per-statement modifier impls for the universal
//! CQL field surface (SRD 73).
//!
//! Each modifier captures the resolved Polydat Value at dispenser-
//! initializer time and applies it via the engine's per-statement
//! setter at execute time.
//!
//! A **batch is a statement too**: the Cassandra C++ driver splits the
//! executable-op surface into distinct `CassStatement` / `CassBatch` C
//! types with parallel setters, but both carry the same execution aspects
//! (consistency, serial consistency, request timeout, tracing). So the
//! aspect target is the [`CassStmt`] trait, implemented for BOTH
//! `cass::Statement` and `cass::Batch`; every modifier is generic over it
//! and the same chain governs single statements and batches alike. Adding
//! an aspect is a trait method the compiler forces every target to
//! implement — a batch can never silently miss one (which is exactly how
//! batch request timeouts were being dropped to the cluster default).
//!
//! Setter naming differs from scylla in two places — this module is the
//! bridge:
//!
//! - `request_timeout_ms` → `set_statement_request_timeout`
//!   (cassandra-cpp uses the `statement_` prefix; scylla just
//!   `request_timeout`).
//! - `page_size` → `set_paging_size` (cassandra-cpp uses "paging";
//!   scylla uses "page").
//!
//! The universal-field names match scylla — the difference is
//! invisible to workload authors.

use std::time::Duration;

use cassandra_cpp as cass;
use nmbrs_runtime::op_modifier::OpFieldModifier;
use polydat::ast::Value;

use crate::common::CqlConsistency;
use crate::common::op_modifier::{CqlModifierFactory, parse_consistency};

// =========================================================================
// Aspect target — the executable CQL op a universal field applies to
// =========================================================================

/// The aspect setter surface shared by every cassandra-cpp executable op.
/// Implemented for `cass::Statement` (raw + prepared dispatch) AND
/// `cass::Batch` — a batch is a statement type like any other, so it carries
/// the same aspects. Universal CQL fields target one of these methods;
/// modifiers are generic over `S: CassStmt`, so one chain applies uniformly to
/// statements and batches.
pub trait CassStmt: Send + Sync + 'static {
    fn set_consistency(&mut self, c: cass::Consistency);
    fn set_serial_consistency(&mut self, c: cass::Consistency);
    fn set_request_timeout(&mut self, timeout: Duration);
    fn set_page_size(&mut self, page_size: i32);
    fn set_tracing(&mut self, tracing: bool);
}

impl CassStmt for cass::Statement {
    fn set_consistency(&mut self, c: cass::Consistency) {
        let _ = cass::Statement::set_consistency(self, c);
    }
    fn set_serial_consistency(&mut self, c: cass::Consistency) {
        let _ = cass::Statement::set_serial_consistency(self, c);
    }
    fn set_request_timeout(&mut self, timeout: Duration) {
        cass::Statement::set_statement_request_timeout(self, Some(timeout));
    }
    fn set_page_size(&mut self, page_size: i32) {
        let _ = cass::Statement::set_paging_size(self, page_size);
    }
    fn set_tracing(&mut self, tracing: bool) {
        let _ = cass::Statement::set_tracing(self, tracing);
    }
}

impl CassStmt for cass::Batch {
    fn set_consistency(&mut self, c: cass::Consistency) {
        let _ = cass::Batch::set_consistency(self, c);
    }
    fn set_serial_consistency(&mut self, c: cass::Consistency) {
        let _ = cass::Batch::set_serial_consistency(self, c);
    }
    fn set_request_timeout(&mut self, timeout: Duration) {
        // The ONLY timeout the driver honours for batch execution — member
        // statements' timeouts are ignored by `cass_session_execute_batch`.
        let _ = cass::Batch::set_request_timeout(self, Some(timeout));
    }
    fn set_page_size(&mut self, _page_size: i32) {
        // A batch is a set of writes; CQL paging applies to reads, so page
        // size is inert for a batch. Intentional no-op — keeps `Batch` a full
        // aspect target without a meaningless setter.
    }
    fn set_tracing(&mut self, tracing: bool) {
        let _ = cass::Batch::set_tracing(self, tracing);
    }
}

// =========================================================================
// Per-field modifier impls (generic over the aspect target)
// =========================================================================

struct ConsistencyMod {
    consistency: cass::Consistency,
    display: &'static str,
}

impl<S: CassStmt> OpFieldModifier<S> for ConsistencyMod {
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
    serial: cass::Consistency,
    display: &'static str,
}

impl<S: CassStmt> OpFieldModifier<S> for SerialConsistencyMod {
    fn field_name(&self) -> &'static str {
        "serial_consistency"
    }
    fn apply(&self, s: &mut S) {
        s.set_serial_consistency(self.serial);
    }
    fn diagnostic_value(&self) -> serde_json::Value {
        serde_json::Value::String(self.display.to_string())
    }
}

struct RequestTimeoutMod {
    timeout: Duration,
}

impl<S: CassStmt> OpFieldModifier<S> for RequestTimeoutMod {
    fn field_name(&self) -> &'static str {
        "request_timeout_ms"
    }
    fn apply(&self, s: &mut S) {
        s.set_request_timeout(self.timeout);
    }
    fn diagnostic_value(&self) -> serde_json::Value {
        serde_json::Value::from(self.timeout.as_millis() as u64)
    }
}

struct PageSizeMod {
    page_size: i32,
}

impl<S: CassStmt> OpFieldModifier<S> for PageSizeMod {
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

impl<S: CassStmt> OpFieldModifier<S> for CqlTraceMod {
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

/// Phantom-typed factory parameterised over the aspect target. Use
/// `CassModifierFactory<cass::Statement>` for raw / prepared dispatch and
/// `CassModifierFactory<cass::Batch>` for batch dispatch — the SAME field
/// resolution produces the SAME chain for either.
pub struct CassModifierFactory<S>(std::marker::PhantomData<fn() -> S>);

impl<S: CassStmt> CqlModifierFactory for CassModifierFactory<S> {
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
                    consistency: cql_to_cass(cl),
                    display: cql_consistency_display(cl),
                })))
            }
            "serial_consistency" => {
                let s = match &value {
                    Value::Str(s) => s.as_ref(),
                    other => return Err(format!("expected str, got {:?}", other.port_type())),
                };
                let (serial, display) = parse_serial_consistency_cass(s)?;
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

fn cql_to_cass(c: CqlConsistency) -> cass::Consistency {
    // Mirrors `to_cass_consistency` in cassandra_cpp/mod.rs — kept
    // local to this module so the universal-field code path is
    // self-contained.
    match c {
        CqlConsistency::Any => cass::Consistency::ANY,
        CqlConsistency::One => cass::Consistency::ONE,
        CqlConsistency::Two => cass::Consistency::TWO,
        CqlConsistency::Three => cass::Consistency::THREE,
        CqlConsistency::Quorum => cass::Consistency::QUORUM,
        CqlConsistency::All => cass::Consistency::ALL,
        CqlConsistency::LocalQuorum => cass::Consistency::LOCAL_QUORUM,
        CqlConsistency::EachQuorum => cass::Consistency::EACH_QUORUM,
        CqlConsistency::LocalOne => cass::Consistency::LOCAL_ONE,
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

/// cassandra-cpp uses a single `Consistency` enum for both normal
/// and serial consistency; `SERIAL` / `LOCAL_SERIAL` are variants
/// of the same enum (unlike scylla's split into `Consistency` +
/// `SerialConsistency`).
fn parse_serial_consistency_cass(s: &str) -> Result<(cass::Consistency, &'static str), String> {
    match s.to_uppercase().as_str() {
        "SERIAL" => Ok((cass::Consistency::SERIAL, "SERIAL")),
        "LOCAL_SERIAL" => Ok((cass::Consistency::LOCAL_SERIAL, "LOCAL_SERIAL")),
        _ => Err(format!(
            "unrecognized serial_consistency '{s}'. Valid: SERIAL, LOCAL_SERIAL"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_err<T>(r: Result<T, String>) -> String {
        match r {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(e) => e,
        }
    }

    #[test]
    fn request_timeout_modifier_builds() {
        let m = CassModifierFactory::<cass::Statement>::modifier_for(
            "request_timeout_ms",
            Value::U64(300_000),
        )
        .unwrap()
        .unwrap();
        assert_eq!(m.field_name(), "request_timeout_ms");
        assert_eq!(m.diagnostic_value(), serde_json::json!(300_000u64));
    }

    #[test]
    fn consistency_modifier_builds() {
        let m = CassModifierFactory::<cass::Statement>::modifier_for(
            "consistency",
            Value::Str(std::sync::Arc::from("LOCAL_QUORUM")),
        )
        .unwrap()
        .unwrap();
        assert_eq!(m.diagnostic_value(), serde_json::json!("LOCAL_QUORUM"));
    }

    #[test]
    fn page_size_rejects_zero() {
        let err = expect_err(CassModifierFactory::<cass::Statement>::modifier_for(
            "page_size",
            Value::U64(0),
        ));
        assert!(err.contains("positive"), "{err}");
    }

    #[test]
    fn cql_trace_accepts_bool() {
        let m =
            CassModifierFactory::<cass::Statement>::modifier_for("cql_trace", Value::Bool(false))
                .unwrap()
                .unwrap();
        assert_eq!(m.diagnostic_value(), serde_json::json!(false));
    }

    #[test]
    fn serial_consistency_parses_levels() {
        let m = CassModifierFactory::<cass::Statement>::modifier_for(
            "serial_consistency",
            Value::Str(std::sync::Arc::from("SERIAL")),
        )
        .unwrap()
        .unwrap();
        assert_eq!(m.diagnostic_value(), serde_json::json!("SERIAL"));
    }

    /// The SAME factory resolves aspects for the batch target — a batch is a
    /// statement too. This type-checks that `CassModifierFactory<cass::Batch>`
    /// produces a `Box<dyn OpFieldModifier<cass::Batch>>`, i.e. every universal
    /// field reaches a batch through the one uniform chain.
    #[test]
    fn factory_builds_for_batch_target() {
        let m: Box<dyn OpFieldModifier<cass::Batch>> =
            CassModifierFactory::<cass::Batch>::modifier_for(
                "request_timeout_ms",
                Value::U64(60_000),
            )
            .unwrap()
            .unwrap();
        assert_eq!(m.field_name(), "request_timeout_ms");
        assert_eq!(m.diagnostic_value(), serde_json::json!(60_000u64));
    }
}
