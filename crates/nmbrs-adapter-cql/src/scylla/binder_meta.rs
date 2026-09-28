// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! CQL parameter-type → polydat `PortType` mapping for the
//! scylla driver.
//!
//! Used at op-template construction by `map_op` to build a
//! [`polydat::binder::Binder::Positional`] from a prepared
//! statement's `get_variable_col_specs()` response. The binder
//! gets verified against the dispenser's parent kernel via
//! [`polydat::binder::verify_against_kernel`] before the
//! dispenser is constructed — so a workload that wires a
//! `VecF32` to a `text` column (the load-bearing bug class)
//! fails at construction rather than silently burning Grisu on
//! every cycle.
//!
//! Mapping policy:
//!
//! - The common scalar types map directly: `Int`/`SmallInt`/`TinyInt`
//!   → `I32` (the smaller-than-32 types widen naturally); `BigInt`
//!   and `Counter` → `I64`; `Float` → `F32`; `Double` → `F64`;
//!   `Boolean` → `Bool`; `Ascii`/`Text` → `Str`; `Blob` → `Bytes`.
//!
//! - `Vector<Float, N>` → `VecF32` and `Vector<Int, N>` → `VecI32`.
//!   Vectors of other element types fall back to `Str` (TODO: add
//!   precise mappings as workloads need them).
//!
//! - The long-tail CQL types (`Uuid`, `Timeuuid`, `Date`, `Time`,
//!   `Timestamp`, `Decimal`, `Varint`, `Duration`, `Inet`,
//!   collections, UDTs, tuples) map to `Str`. This is the
//!   permissive policy you confirmed for now: the binder
//!   verification's `Str`-lvalue rule permits any rvalue, so
//!   workloads that bind these don't get blocked; the slot is
//!   effectively un-typed at the construction-time check.
//!   Concrete mappings can land per type as workloads need them
//!   (e.g. `Uuid` → a future `PortType::Uuid`). Fallback slots
//!   are surfaced via the second return value of [`cql_to_polydat`]
//!   so `map_op` can WARN about them — the operator sees which
//!   slots have lost typed verification.

use polydat::ast::PortType;
use scylla::cluster::metadata::{ColumnType, NativeType};

/// Result of mapping one CQL `ColumnType` to a polydat
/// `PortType`. The `Option<String>` carries the human-readable
/// CQL type label when this slot fell back to a permissive
/// mapping (currently always `PortType::Str`) because no
/// precise polydat equivalent has been wired yet. `None` means
/// the mapping is exact and no warning is warranted.
pub type Mapping = (PortType, Option<String>);

/// Map a scylla `ColumnType` to a polydat `PortType`, plus an
/// optional CQL-type-label for the fallback warning.
///
/// See the module docs for the per-type policy. Fallback slots
/// return `Some("<cql-type-label>")` as the second tuple element
/// so the caller (typically `map_op`) can emit a WARN with the
/// op + slot identity — making it clear which positions in a
/// prepared statement have *not* been typed-checked precisely
/// at this binder-verification step.
pub fn cql_to_polydat(typ: &ColumnType<'_>) -> Mapping {
    match typ {
        ColumnType::Native(n) => match n {
            // Exact mappings — no warning.
            NativeType::Boolean => (PortType::Bool, None),
            NativeType::Int => (PortType::I32, None),
            NativeType::BigInt => (PortType::I64, None),
            NativeType::Counter => (PortType::I64, None),
            NativeType::Float => (PortType::F32, None),
            NativeType::Double => (PortType::F64, None),
            NativeType::Ascii => (PortType::Str, None),
            NativeType::Text => (PortType::Str, None),
            NativeType::Blob => (PortType::Bytes, None),
            // Widened mappings — represented natively as a wider
            // polydat type but the semantics line up; no warning.
            NativeType::SmallInt => (PortType::I32, None),
            NativeType::TinyInt => (PortType::I32, None),
            // Long-tail native types: precise polydat mappings
            // not yet wired. Fallback → Str with a warning so the
            // operator can see which slots lost typed checking.
            other => (PortType::Str, Some(format!("{other:?}"))),
        },
        // Native-element vectors map directly; vectors of other
        // element types fall back to Str (TODO: vector<text>,
        // vector<bigint>, etc. once workloads need them).
        ColumnType::Vector { typ, dimensions } => match typ.as_ref() {
            ColumnType::Native(NativeType::Float) => (PortType::VecF32, None),
            ColumnType::Native(NativeType::Int) => (PortType::VecI32, None),
            other_elem => (
                PortType::Str,
                Some(format!("Vector<{other_elem:?}, {dimensions}>")),
            ),
        },
        // Collections / UDTs / tuples have no current polydat
        // typed equivalent. Str fallback per module-doc policy.
        ColumnType::Collection { typ, .. } => (PortType::Str, Some(format!("Collection<{typ:?}>"))),
        ColumnType::UserDefinedType { definition, .. } => (
            PortType::Str,
            Some(format!("UserDefinedType<{}>", definition.name)),
        ),
        ColumnType::Tuple(elems) => (
            PortType::Str,
            Some(format!("Tuple<{} elements>", elems.len())),
        ),
        // `ColumnType` is `#[non_exhaustive]`; future-added
        // variants land here. Str fallback keeps verification
        // working until a precise mapping is added.
        other => (PortType::Str, Some(format!("{other:?}"))),
    }
}
