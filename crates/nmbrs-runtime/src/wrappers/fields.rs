// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Fields wrapper — prints the rendered op text per cycle.
//!
//! Two modes, distinguished by whether op_fields are attached at
//! wrap time:
//! - **Adapter-agnostic field render** (`fields: true` on op template):
//!   resolves the op_fields the template carries via the
//!   per-fiber wires, prints each value verbatim, then forwards
//!   the call to inner. This is the canonical "render what the
//!   adapter would send" surface — under `dryrun=fields` the
//!   DRYRUN wrapper short-circuits inner, so this wrapper is the
//!   only surface that prints.
//! - **Body+wire dump** (no op_fields at wrap): falls back to
//!   the inner result body + wire snapshot. Kept as the
//!   historical surface for callers that want post-execute
//!   introspection rather than pre-execute rendering.

use std::sync::Arc;

use crate::adapter::WrappingDispenser;
use crate::adapter::{ExecutionError, OpDispenser, OpResult};
use crate::wrapper_registry::{WrapperName, WrapperRegistration, WrapperSubject};

/// SRD-32a wrapper name.
pub const NAME: WrapperName = WrapperName::new("fields");

/// Trigger: op carries `fields: true` (bool, or string "true").
fn triggers(s: WrapperSubject) -> bool {
    let Some(template) = s.op() else {
        return false;
    };
    template
        .params
        .get("fields")
        .map(|v| {
            v.as_bool()
                .unwrap_or_else(|| v.as_str().map(|s| s == "true").unwrap_or(false))
        })
        .unwrap_or(false)
}

fn describe_assignment(_: WrapperSubject) -> Option<String> {
    Some("fields: rendered op text to stdout".into())
}

inventory::submit! {
    WrapperRegistration {
        name: NAME,
        owned_fields: &["fields"],
        triggers,
        // SRD-32a's table lists `fields.requires_inner = [result]`,
        // but the cascade composes `result` OUTSIDE `fields`
        // (innermost-first list ends `..., fields, result,
        // metrics`). Declaring `requires_inner = [result]` would
        // make `result` innermore than `fields`, which contradicts
        // the cascade and breaks the byte-identical-output test
        // bar in §"Migration".
        requires_inner: &[],
        forbids_outer: &[],
        mutually_exclusive_with: &[],
        describe_assignment,
        levels: &[crate::wrapper_registry::WrapperLevel::Op],
    }
}

/// Wraps any adapter's dispenser and prints the result body to stdout
/// as JSON after each execution. Adapter-agnostic — works with CQL,
/// HTTP, stdout, or any adapter that returns a ResultBody.
///
/// Enabled by wrapping at init time when `dryrun=fields` is active
/// or when the op has `fields: true`.
pub struct FieldsDispenser {
    inner: Arc<dyn OpDispenser>,
    op_name: String,
    /// Op-field key/value pairs cloned from the op template at
    /// wrap time. When non-empty, this wrapper resolves them via wires
    /// at each cycle and prints the value strings — that's the
    /// pre-execute "render what would have been sent" surface.
    /// When empty, falls back to the post-execute body
    /// + wires dump for callers that want introspection only.
    op_fields: Vec<(String, serde_json::Value)>,
}

impl FieldsDispenser {
    pub fn wrap(inner: Arc<dyn OpDispenser>, op_name: &str) -> Arc<dyn OpDispenser> {
        Arc::new(Self {
            inner,
            op_name: op_name.to_string(),
            op_fields: Vec::new(),
        })
    }

    /// Same as [`Self::wrap`], but seeds the dispenser with the
    /// op_fields it should resolve + print on each cycle. Used
    /// at activity init when the op template's `op:` map is
    /// known and we want the rendered op text on stdout (e.g.
    /// under `dryrun=fields`).
    pub fn wrap_with_op_fields(
        inner: Arc<dyn OpDispenser>,
        op_name: &str,
        op_fields: Vec<(String, serde_json::Value)>,
    ) -> Arc<dyn OpDispenser> {
        Arc::new(Self {
            inner,
            op_name: op_name.to_string(),
            op_fields,
        })
    }
}

impl WrappingDispenser for FieldsDispenser {}

impl OpDispenser for FieldsDispenser {
    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a crate::fixture::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            // Pre-execute rendering mode — resolve the
            // captured op_fields and print their rendered
            // values. This runs BEFORE inner.execute so the
            // surface lands the rendered op text whether or
            // not inner short-circuits (DRYRUN under
            // dryrun=fields, etc).
            if !self.op_fields.is_empty() {
                match crate::wires::resolve_op_fields_via_wires(&self.op_fields, ctx.wires) {
                    Ok(resolved) => {
                        for s in resolved.strings().iter() {
                            println!("{s}");
                        }
                    }
                    Err(msg) => {
                        eprintln!("[{}@{}] fields-render failed: {msg}", self.op_name, cycle);
                    }
                }
            }

            let result = self.inner.execute(cycle, ctx).await?;

            // Post-execute introspection — only fires when
            // this wrapper was wrapped without op_fields (the
            // body+wires fallback surface). With op_fields the
            // pre-execute render is the authoritative surface
            // and the post-execute dump is omitted to keep
            // stdout terse.
            if self.op_fields.is_empty() {
                if let Some(ref body) = result.body {
                    let json = body.to_json();
                    println!(
                        "[{}@{}] {} rows: {}",
                        self.op_name,
                        cycle,
                        body.element_count(),
                        serde_json::to_string_pretty(&json).unwrap_or_else(|_| json.to_string())
                    );
                } else {
                    println!("[{}@{}] (no result body)", self.op_name, cycle);
                }
                for name in ctx.wires.names() {
                    if let Some(value) = ctx.wires.get(&name) {
                        println!("  wire {name} = {}", value.to_display_string());
                    }
                }
            }

            Ok(result)
        })
    }
    fn inner_dispenser(&self) -> Option<&dyn OpDispenser> {
        Some(self.inner.as_ref())
    }
}
