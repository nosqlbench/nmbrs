// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Op-mode dispatch for CQL adapters.
//!
//! Every CQL engine reads the same op fields to decide how to run
//! a statement: `raw:` and `simple:` skip the prepared cache,
//! `prepared:` and `stmt:` go through the prepared statement
//! path, and a `batch:` param promotes the prepared path into a
//! BATCH executor.

/// Op fields recognized as the statement carrier, in priority
/// order. The first one present in the op selects the dispatch
/// mode.
///
/// - `raw` / `simple` → unprepared / direct-execute statement.
/// - `prepared` / `stmt` → prepared statement (cached per op).
///
/// A `batch:` param on the op overlays the prepared path with
/// batch dispatch.
pub const STMT_FIELD_NAMES: &[&str] = &["raw", "simple", "prepared", "stmt"];

/// The dispatch mode an op falls into.
///
/// Each engine adapter resolves an op to one of these once at
/// `map_op` time, then constructs the matching dispenser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpMode {
    /// Unprepared, direct execute. Selected by `raw:` or
    /// `simple:` field.
    Raw,
    /// Prepared statement, executed once per cycle. Selected by
    /// `prepared:` or `stmt:` field.
    Prepared,
    /// Prepared statement bound N times into one CQL BATCH.
    /// Selected by the presence of a `batch:` param on a
    /// prepared-mode op.
    Batch,
}

impl OpMode {
    /// Decide the mode from the matched statement field name and
    /// whether the op declares a `batch:` param.
    ///
    /// Field-name semantics:
    /// - `raw` / `simple`: always [`OpMode::Raw`] (batch is
    ///   ignored — CQL's batch protocol takes prepared statements).
    /// - `prepared` / `stmt`: [`OpMode::Batch`] if `has_batch_param`,
    ///   else [`OpMode::Prepared`].
    pub fn from_stmt_field(stmt_field: &str, has_batch_param: bool) -> Self {
        match stmt_field {
            "raw" | "simple" => Self::Raw,
            _ if has_batch_param => Self::Batch,
            _ => Self::Prepared,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_or_simple_always_raw() {
        assert_eq!(OpMode::from_stmt_field("raw", false), OpMode::Raw);
        assert_eq!(OpMode::from_stmt_field("raw", true), OpMode::Raw);
        assert_eq!(OpMode::from_stmt_field("simple", true), OpMode::Raw);
    }

    #[test]
    fn prepared_with_batch_promotes() {
        assert_eq!(OpMode::from_stmt_field("prepared", false), OpMode::Prepared);
        assert_eq!(OpMode::from_stmt_field("prepared", true), OpMode::Batch);
        assert_eq!(OpMode::from_stmt_field("stmt", true), OpMode::Batch);
    }

    #[test]
    fn stmt_field_names_priority_order() {
        // raw / simple precede prepared / stmt — if a workload
        // accidentally sets both, the unprepared form wins, which
        // is the safer fallback (no schema-metadata dependency).
        assert_eq!(STMT_FIELD_NAMES, &["raw", "simple", "prepared", "stmt"]);
    }
}
