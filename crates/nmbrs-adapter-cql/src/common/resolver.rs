// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Adapter registration for `cql` and driver dispatch.
//!
//! The `cql` adapter is the single user-facing CQL adapter.
//! Internally it has multiple driver implementations
//! (`scylla` — pure Rust; `cassandra-cpp` — Apache Cassandra C++ via
//! FFI), each registered as a [`DriverImpl`]
//! (nmbrs_runtime::adapter::DriverImpl) under
//! `adapter = "cql"`. Driver names are *internal* — they're
//! never user-facing adapter names.
//!
//! At session start the runner walks the registered
//! [`DriverImpl`]s for `cql`, picks one (user override via
//! `cqldriver=…`, or default by ascending
//! [`DriverImpl::default_rank`]), and instantiates it. The
//! returned [`DriverAdapter`](nmbrs_runtime::adapter::DriverAdapter)
//! reports its name as `"cql"` regardless of which driver
//! backs it.
//!
//! User-facing examples:
//!
//! ```text
//! nmbrs run adapter=cql                            # default driver
//! nmbrs run adapter=cql cqldriver=scylla          # force scylla
//! nmbrs run adapter=cql cqldriver=cassandra-cpp   # force cassandra-cpp
//! ```

use nmbrs_runtime::adapter::{AdapterRegistration, DisplayPreference, instantiate_with_driver};
use nmbrs_runtime::control_catalog::{ControlDesc, ControlValueType, DeclaredWhen};

/// The workload-param name a user sets to pick a specific CQL
/// driver, overriding the rank-derived default. Single value
/// (not a list) — name one driver.
pub const CQL_DRIVER_PARAM: &str = "cqldriver";

/// SRD-23 — the `cql_trace_rate` dynamic control's capability descriptor.
/// The single source of truth: the live control declared in
/// [`declare_controls`](nmbrs_runtime::adapter::DriverAdapter::declare_controls)
/// derives its name / range / gauge from this (via [`ControlDesc::build_f64`]),
/// and `nmbrs describe controls` reads it without constructing the adapter.
pub const CQL_TRACE_RATE: ControlDesc = ControlDesc {
    name: "cql_trace_rate",
    value_type: ControlValueType::Fraction,
    default: 0.0,
    min: 0.0,
    max: 1.0,
    unit: "probability",
    doc: "Fraction of CQL ops to request server-side tracing for (0 = off, 1 = all).",
    declared_when: DeclaredWhen::Driver("cassandra-cpp"),
};

/// The dynamic controls the `cql` adapter can declare *in this build*. Only the
/// `cassandra-cpp` driver declares `cql_trace_rate`, so a scylla-only binary
/// advertises none — `describe controls` reflects what the binary can actually
/// do, not an aspirational superset.
fn cql_supported_controls() -> &'static [ControlDesc] {
    #[cfg(feature = "engine-cassandra-cpp")]
    {
        &[CQL_TRACE_RATE]
    }
    #[cfg(not(feature = "engine-cassandra-cpp"))]
    {
        &[]
    }
}

// `cql` adapter registration. The factory delegates to
// `instantiate_with_driver`, which picks among the registered
// `DriverImpl`s for `adapter = "cql"` (user override via
// `cqldriver=…`, otherwise the lowest-rank driver compiled in).
//
// Driver-specific params (`hosts`, `port`, `keyspace`, ...) are
// declared on each `DriverImpl` and unioned into the adapter's
// known-params surface by `registered_adapter_params()`; this
// registration only declares the driver-selector itself.
inventory::submit! {
    AdapterRegistration {
        names: || &["cql"],
        known_params: || &[CQL_DRIVER_PARAM],
        display_preference: |_params| DisplayPreference::Auto,
        supported_controls: cql_supported_controls,
        create: |params| Box::pin(instantiate_with_driver("cql", CQL_DRIVER_PARAM, params)),
    }
}

/// Echoed in startup banners. Returns the rank-sorted list of
/// CQL driver implementations compiled into this binary. Empty
/// when no driver feature is enabled.
pub fn default_cql_drivers() -> Vec<&'static str> {
    nmbrs_runtime::adapter::default_drivers("cql")
}

/// Convenience helper for diagnostic surfaces (CLI banner, web
/// dashboard, error messages). Joins the driver names with `, `.
pub fn default_cql_drivers_display() -> String {
    let drivers = default_cql_drivers();
    if drivers.is_empty() {
        "(none — build with engine-scylla / engine-cassandra-cpp)".into()
    } else {
        drivers.join(", ")
    }
}
