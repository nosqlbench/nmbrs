// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Engine-agnostic CQL surface shared by every engine module.
//!
//! Each engine — `cassandra_cpp` (Apache Cassandra C++ driver), `scylla`
//! (pure-Rust driver), and any future addition — implements its
//! own [`nmbrs_runtime::adapter::DriverAdapter`] but consumes this
//! module for everything that isn't transport-specific:
//!
//! - [`CqlConfig`] and its [`from_params`](CqlConfig::from_params)
//!   parser. Every engine reads the same workload params.
//! - [`CqlConsistency`] — engine-agnostic consistency enum + parser.
//!   Each engine maps to its driver's native consistency type.
//! - [`OpMode`] and [`STMT_FIELD_NAMES`] — the op-field naming
//!   convention that selects raw vs prepared vs batch dispatch.
//! - The `cql_timeuuid` Polydat node (macro-authored in [`nodes`]),
//!   registered once via inventory and available wherever this adapter is
//!   linked.
//! - [`default_status_metrics`] — the `rows_inserted` rate metric
//!   surfaced in the TUI status line.
//! - [`resolver`] — the `cql` adapter registration. The
//!   factory walks the registered `DriverImpl`s for `cql` and
//!   instantiates one based on the user's `cqldriver=…`
//!   selector (or default rank).
//!
//! Pure Rust, no driver dependencies. Always compiled
//! regardless of which driver features are enabled.

pub mod config;
pub mod nodes;
pub mod op_modifier;
pub mod opmode;
pub mod resolver;
pub mod session_handle;
pub mod size_estimator;
pub mod status;

pub use config::{CqlConfig, CqlConsistency};
pub use opmode::{OpMode, STMT_FIELD_NAMES};
pub use resolver::CQL_TRACE_RATE;
pub use session_handle::{CqlSessionHandle, CqlSettingsSource};
pub use status::default_status_metrics;

/// Whether a raw CQL driver error string denotes a TRANSIENT condition a
/// retry may clear — request / coordinator timeouts, overload, or unavailable
/// replicas. Permanent errors (syntax, invalid query, auth, unprepared) are
/// NOT retryable: retrying them only burns the `retries:` budget and delays
/// the real failure.
///
/// Engine-agnostic: matches the RAW driver error text of either engine
/// (case-insensitively), evaluated BEFORE the statement text is appended, so a
/// statement that happens to contain one of these words can't cause a
/// false-positive. Consumed at each engine's `cql_error` execute site to set
/// [`nmbrs_runtime::adapter::AdapterError::retryable`], which the runtime's
/// `RetryDispenser` honours.
pub fn cql_error_is_retryable(raw: &str) -> bool {
    let u = raw.to_ascii_uppercase();
    u.contains("TIMED_OUT")      // cassandra `LIB_REQUEST_TIMED_OUT`
        || u.contains("TIMEOUT")     // scylla RequestTimeout / server READ_/WRITE_TIMEOUT
        || u.contains("OVERLOAD")    // OVERLOADED
        || u.contains("UNAVAILABLE") // not enough replicas — a retry may find them
}

/// Whether a CQL statement is RETRY-SAFE — re-executing it after an
/// ambiguous failure (a timeout whose write may or may not have landed)
/// cannot change the outcome. This is the STATEMENT's property, orthogonal
/// to the error's transience ([`cql_error_is_retryable`]): an op retries
/// only when the statement is retry-safe AND the error is transient.
///
/// - Plain upserts / deletes keyed by primary key are idempotent —
///   re-applying converges to the same row state. Retry-safe.
/// - LWT / conditional statements (`IF EXISTS`, `IF NOT EXISTS`,
///   `IF col = …`) are NOT: the condition may observe the first attempt's
///   applied-but-unacked effect and resolve differently on replay.
///
/// Counter mutations are the other non-idempotent class; the batch
/// dispensers exclude them by TYPE (`batchtype: counter`), not by text.
///
/// A batch DERIVES its retry-safety from its inner statements. The batch
/// dispensers carry ONE uniform prepared statement (SRD-22 stride), so the
/// derivation is computed ONCE at dispenser init — never per stanza or per
/// cycle.
pub fn cql_statement_retry_safe(stmt: &str) -> bool {
    // Word-ish LWT detection: ` IF ` as a token covers `IF EXISTS`,
    // `IF NOT EXISTS`, and `IF col = …`. A false NEGATIVE here only
    // disables retries for that statement; it can never cause a
    // non-idempotent statement to be retried.
    !stmt.to_ascii_uppercase().contains(" IF ")
}

#[cfg(test)]
mod retryable_tests {
    use super::{cql_error_is_retryable, cql_statement_retry_safe};
    #[test]
    fn transient_errors_retry_permanent_do_not() {
        assert!(cql_error_is_retryable(
            "LIB_REQUEST_TIMED_OUT: Request timed out"
        ));
        assert!(cql_error_is_retryable("Request timeout"));
        assert!(cql_error_is_retryable("Database returned: WRITE_TIMEOUT"));
        assert!(cql_error_is_retryable("Overloaded"));
        assert!(cql_error_is_retryable(
            "Not enough replicas available: UNAVAILABLE"
        ));
        assert!(!cql_error_is_retryable(
            "Invalid query: unconfigured table foo"
        ));
        assert!(!cql_error_is_retryable("Syntax error at line 1"));
    }

    #[test]
    fn plain_upserts_are_retry_safe_lwt_is_not() {
        assert!(cql_statement_retry_safe(
            "INSERT INTO ks.t (key, value) VALUES (?, ?)"
        ));
        assert!(cql_statement_retry_safe(
            "UPDATE ks.t SET value = ? WHERE key = ?"
        ));
        assert!(!cql_statement_retry_safe(
            "INSERT INTO ks.t (key, value) VALUES (?, ?) IF NOT EXISTS"
        ));
        assert!(!cql_statement_retry_safe(
            "UPDATE ks.t SET value = ? WHERE key = ? IF value = ?"
        ));
        assert!(!cql_statement_retry_safe(
            "DELETE FROM ks.t WHERE key = ? IF EXISTS"
        ));
    }
}
