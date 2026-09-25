// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! # nmbrs-adapter-openapi
//!
//! OpenAPI workload-source adapter for nmbrs. Reads an OpenAPI
//! 3.x spec, discovers endpoints, and synthesizes [`ParsedOp`]s
//! plus Polydat binding source so a runner can drive traffic against
//! the target API through the standard `http` adapter.
//!
//! Unlike a transport adapter (`stdout`, `cql`, `http`), this
//! crate is a *workload generator*. It doesn't implement
//! [`DriverAdapter`](nmbrs_runtime::adapter::DriverAdapter); it
//! produces synthesized ops that the http adapter executes.
//!
//! ## Usage
//!
//! ```ignore
//! let yaml = std::fs::read_to_string("petstore.yaml")?;
//! let (api, ops) = nmbrs_adapter_openapi::parse_spec(&yaml)?;
//! let (parsed_ops, bindings) =
//!     nmbrs_adapter_openapi::generate_ops(&ops, "http://localhost:8080");
//! ```
//!
//! `openapi-ops` (this crate's binary) renders a spec as a library of
//! reusable http op templates instead ([`library`]): a workload
//! `extends:` the library and instantiates operations with `uses:`,
//! supplying the fields each one needs.
//!
//! The nmbrs binary integrates this via its `openapi` feature:
//!
//! ```text
//! nmbrs --features openapi describe spec=petstore.yaml
//! nmbrs --features openapi run spec=petstore.yaml base_url=http://localhost:8080
//! ```

pub mod library;
pub mod spec;
pub mod workload;

pub use library::render_op_library;
pub use spec::{ApiOperation, BodyInfo, FieldInfo, ParamInfo, describe_operations, parse_spec};
pub use workload::generate_ops;
