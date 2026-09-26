// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Prepared-statement dispenser.
//!
//! Used for `prepared:` / `stmt:` op fields without a `batch:`
//! param. The statement is prepared in `map_op` (the
//! dispenser-init stack frame — see
//! `nmbrs_runtime::adapter::DriverAdapter::map_op` docs) and
//! handed in already prepared so per-cycle execute has no
//! init work left to do.

use std::sync::Arc;

use nmbrs_runtime::adapter::{ExecutionError, OpDispenser, OpResult, ResultBody};
use polydat::ast::Value;
use scylla::client::session::Session;
use scylla::statement::prepared::PreparedStatement;

use super::{ScyllaResultBody, binders, format_cql_error, op_error};

pub(super) struct ScyllaPreparedDispenser {
    /// The op-template kernel this dispenser was mapped against
    /// (SRD-13d §"op templates are scopes").
    ///
    /// Load-bearing, not decorative: the executor materialises one per-fiber
    /// kernel per dispenser FROM this program, and the wrapper-side `PullPlan`
    /// is built against the very same op-template program. Returning `None`
    /// here leaves the per-op slot empty, the plan falls back to the phase
    /// kernel, and its output indices then address the WRONG program — reading
    /// a neighbouring metric where the index happens to be in range and
    /// panicking the pull plan where it is not.
    canonical_kernel: Arc<dyn polydat::Kernel>,
    session: Arc<Session>,
    /// Pre-prepared statement (consistency + per-op modifiers
    /// already applied at `map_op` time). Wrapped in `Arc` so
    /// the per-fiber dispatch can share the prep result without
    /// re-preparing.
    prepared: Arc<PreparedStatement>,
    /// Statement text — retained for error diagnostics only;
    /// not used on the hot path.
    stmt_text: String,
    /// Bind-point names in `?` order, captured from the statement
    /// text at map-op time. Each cycle, we look up the value at
    /// each bind position by name through `wires`.
    bind_names: Vec<String>,
}

impl ScyllaPreparedDispenser {
    pub fn new(
        canonical_kernel: Arc<dyn polydat::Kernel>,
        session: Arc<Session>,
        prepared: Arc<PreparedStatement>,
        stmt_text: String,
        bind_names: Vec<String>,
    ) -> Self {
        Self {
            session,
            prepared,
            stmt_text,
            bind_names,
            canonical_kernel,
        }
    }
}

impl OpDispenser for ScyllaPreparedDispenser {
    fn canonical_kernel(&self) -> Option<&std::sync::Arc<dyn polydat::Kernel>> {
        Some(&self.canonical_kernel)
    }
    fn execute<'a>(
        &'a self,
        _cycle: u64,
        ctx: &'a nmbrs_runtime::adapter::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        let wires = ctx.wires;
        Box::pin(async move {
            // SRD-68 Push 5: pull values by bind-point name in `?`
            // order through the generic wires API. Empty string is
            // the legacy fallback for an unresolved bind name; the
            // Polydat compiler should have provisioned every name, but
            // an absent name shouldn't fail-stop the cycle.
            let bind_values: Vec<Value> = self
                .bind_names
                .iter()
                .map(|name| wires.get(name).unwrap_or(Value::Str(String::new().into())))
                .collect();
            let col_specs = self.prepared.get_variable_col_specs();
            let row = binders::build_row(col_specs, &bind_values)
                .map_err(|e| op_error("bind_error", e, false))?;

            let result = self
                .session
                .execute_unpaged(&self.prepared, row)
                .await
                .map_err(|e| {
                    op_error(
                        "cql_error",
                        format_cql_error(&e.to_string(), &self.stmt_text),
                        crate::common::cql_error_is_retryable(&e.to_string()),
                    )
                })?;

            let body = ScyllaResultBody::from_query_result(result);
            let body_box: Option<Box<dyn ResultBody>> = if body.element_count() > 0 {
                Some(Box::new(body))
            } else {
                None
            };
            Ok(OpResult {
                body: body_box,
                skipped: false,
            })
        })
    }
}
