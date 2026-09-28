// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! # nmbrs-adapter-cql
//!
//! Multi-engine CQL adapter for nmbrs. Each engine is a sibling
//! submodule gated behind a Cargo feature; both can be linked
//! into the same binary, in which case the user picks at runtime
//! via the `cqldriver=<name>` workload parameter (or
//! `adapter=<engine-name>` for direct dispatch).
//!
//! At least one engine feature must be enabled.

#[cfg(not(any(feature = "engine-scylla", feature = "engine-cassandra-cpp")))]
compile_error!(
    "nmbrs-adapter-cql needs at least one engine feature enabled; \
     try --features engine-scylla (default) or --features engine-cassandra-cpp"
);

// Engine-agnostic surface (config / consistency / op-mode / GK
// nodes / `cql` alias resolver). Always compiled — the resolver's
// inventory submission is what makes `adapter=cql` work even
// when only a single engine feature is linked.
pub mod common;

#[cfg(feature = "engine-scylla")]
pub mod scylla;

#[cfg(feature = "engine-cassandra-cpp")]
pub mod cassandra_cpp;
