// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Raw / unprepared statement dispenser.
//!
//! Used for `raw:` / `simple:` op fields. The statement text is
//! resolved at cycle time (bind points already substituted by
//! the runtime) and handed straight to `session.query_unpaged`.
//! Skips the prepared-statement cache; appropriate for one-shot
//! DDL phases (`CREATE KEYSPACE`, `DROP TABLE`, …) and
//! diagnostic runs where the per-prepare cost would dominate.

use std::sync::Arc;

use nmbrs_runtime::adapter::{ExecutionError, OpDispenser, OpResult, ResultBody};
use nmbrs_runtime::op_modifier::ModifierChain;
use scylla::client::session::Session;
use scylla::statement::{Consistency, Statement};

use super::{ScyllaResultBody, format_cql_error, op_error};

pub(super) struct ScyllaRawDispenser {
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
    consistency: Consistency,
    /// Original statement template, with `{name}` placeholders
    /// intact. Rendered at cycle time through the generic GK
    /// wires API (SRD-68 Push 5).
    stmt_template: String,
    /// SRD 73 universal per-op field overrides, built once at
    /// `map_op` time by walking the Polydat scope. The session-level
    /// `consistency` is set first below; the chain then layers
    /// per-op overrides on top.
    modifiers: ModifierChain<Statement>,
}

impl ScyllaRawDispenser {
    pub fn new(
        canonical_kernel: Arc<dyn polydat::Kernel>,
        session: Arc<Session>,
        consistency: Consistency,
        stmt_template: String,
        modifiers: ModifierChain<Statement>,
    ) -> Self {
        Self {
            session,
            consistency,
            stmt_template,
            modifiers,
            canonical_kernel,
        }
    }
}

impl OpDispenser for ScyllaRawDispenser {
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
            let text = nmbrs_runtime::wires::substitute_via_wires(&self.stmt_template, wires)
                .map_err(|e| op_error("bind_error", e, false))?;

            let mut stmt = Statement::new(text.clone());
            stmt.set_consistency(self.consistency);
            // SRD 73: layer per-op universal-field overrides on top
            // of the session-level consistency. Hot-path no-op when
            // the user didn't bind any per-op field.
            self.modifiers.apply(&mut stmt);

            let result = self.session.query_unpaged(stmt, ()).await.map_err(|e| {
                op_error(
                    "cql_error",
                    format_cql_error(&e.to_string(), &text),
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
