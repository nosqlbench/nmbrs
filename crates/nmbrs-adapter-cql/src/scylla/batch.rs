// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Batch dispenser.
//!
//! Used for `prepared:` / `stmt:` ops with a `batch:` row cap and/or
//! a `max_batch_size:` byte budget (SRD-103 §6). Prepares the
//! statement once and caches it; on each `execute` call it pulls a
//! deterministic number of rows (advancing the per-fiber wire coord
//! per row, SRD-68) and packs them into one or more CQL `Batch`
//! round-trips.
//!
//! Fill strategy (see [`crate::common::size_estimator`]):
//! - `batch: N` alone → exactly `N` rows in one batch (today's model).
//! - `max_batch_size` alone → a byte-budgeted batch whose row count
//!   is predicted from the first row's estimated encoded size.
//! - both → `N` rows total, split into sub-batches each held under
//!   the byte budget; whichever limit hits first flushes.
//! - neither → a single row.
//!
//! One op invocation may therefore emit multiple batch round-trips;
//! the work is summed into a single [`OpResult`] and a failure on any
//! sub-batch fails the whole op.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use nmbrs_runtime::adapter::{ExecutionError, OpDispenser, OpResult, ResultBody};
use polydat::ast::Value;
use scylla::client::session::Session;
use scylla::statement::{
    Consistency,
    batch::{Batch, BatchType},
    prepared::PreparedStatement,
};

use super::{ScyllaResultBody, binders, format_cql_error, op_error};
use crate::common::size_estimator;

pub(super) struct ScyllaBatchDispenser {
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
    /// Pre-prepared inner statement (consistency + modifiers
    /// already applied at `map_op` time). Same dispenser-init
    /// contract as [`super::prepared::ScyllaPreparedDispenser`].
    prepared: Arc<PreparedStatement>,
    /// Statement text — retained for error diagnostics only.
    stmt_text: String,
    /// Bind-point names in `?` order. Each row's values come from
    /// `wires.get(name)` after `wires.advance(coord)` per row
    /// (SRD-68 invariant: "each iteration of the batch is
    /// considered another pull").
    bind_names: Vec<String>,
    /// FIXED uniform batch stride — the nominal number of cursor
    /// ordinals one invocation reads AND advances (SRD-22 cover-once).
    /// Settled once at `map_op` from `batch:` / `max_batch_size` /
    /// the characterized row size (see [`size_estimator::fixed_batch_stride`]).
    /// Reported via [`OpDispenser::rows_per_op`] so the executor drives
    /// the phase cursor with `Σ rows_per_op`; the ACTUAL per-invocation
    /// row count is `ExecCtx::run_len` (== `n` except at the short tail).
    n: usize,
    /// SRD-103 §6 byte budget (`max_batch_size`, a literal
    /// magnitude). `None` → no byte cap. Retained as the sub-batch
    /// flush safety valve (a fixed run should already fit, but a row
    /// that alone exceeds the budget is split off defensively).
    max_batch_bytes: Option<u64>,
    batch_type: BatchType,
    /// Batch-level consistency — applied to the [`Batch`] itself
    /// per execute. Distinct from the consistency baked into the
    /// inner prepared statement: scylla's batch wrapper has its
    /// own consistency setter that overrides the per-statement
    /// value when both are present.
    consistency: Consistency,
    /// Fires the "single row exceeds max_batch_size" warning at most
    /// once per dispenser lifetime.
    oversize_warned: AtomicBool,
    /// Cumulative rows written across every successful op — surfaced
    /// as the `rows_inserted` status counter (mirrors the per-op
    /// `ctx.wires.write("rows_inserted", …)`, which is only visible to
    /// wrappers on the same cycle; the display reads the aggregate
    /// through [`OpDispenser::status_counters`]).
    rows_total: AtomicU64,
    /// Cumulative count of batch ops that actually wrote ≥1 row — one
    /// inc per successful `execute`. The display divides
    /// `rows_inserted` by this to show the true average batch size,
    /// instead of `rows_inserted / stanzas_total`, whose denominator
    /// counts every op attempt (retries, failures, non-inserting ops).
    batch_writes: AtomicU64,
    /// SRD 73 universal per-op field overrides, resolved once at `map_op`.
    /// A batch is a statement too, so these apply to the [`Batch`] itself
    /// (consistency / serial / request timeout / tracing) — the aspects
    /// that govern batch execution, uniform with the single-statement path.
    modifiers: nmbrs_runtime::op_modifier::ModifierChain<Batch>,
    /// Derived ONCE at dispenser init from the batch's inner statements
    /// (uniform template with stride ⇒ one prepared statement): `false` for
    /// counter batches and LWT statements, `true` for plain PK-keyed
    /// upserts. Gates the transient-error retry classification. See
    /// `cql_statement_retry_safe`.
    retry_safe: bool,
}

impl ScyllaBatchDispenser {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        canonical_kernel: Arc<dyn polydat::Kernel>,
        session: Arc<Session>,
        prepared: Arc<PreparedStatement>,
        stmt_text: String,
        bind_names: Vec<String>,
        n: usize,
        max_batch_bytes: Option<u64>,
        batch_type: BatchType,
        consistency: Consistency,
        modifiers: nmbrs_runtime::op_modifier::ModifierChain<Batch>,
        retry_safe: bool,
    ) -> Self {
        Self {
            session,
            prepared,
            stmt_text,
            bind_names,
            n: n.max(1),
            max_batch_bytes,
            batch_type,
            consistency,
            oversize_warned: AtomicBool::new(false),
            rows_total: AtomicU64::new(0),
            batch_writes: AtomicU64::new(0),
            modifiers,
            retry_safe,
            canonical_kernel,
        }
    }

    /// Execute one CQL `Batch` over the given row-value sets. Returns
    /// the query result body ONLY when it carries returned rows (e.g.
    /// the `[applied]` status of a conditional/LWT batch); a plain
    /// write acknowledgment returns `None` here — the rows-written
    /// count is summed as `submitted` in `execute` and carried by a
    /// single [`ScyllaResultBody::write_ack`] for the whole op.
    /// Shared by every sub-batch flush.
    async fn flush_sub_batch(
        &self,
        row_value_sets: &[Vec<Value>],
    ) -> Result<Option<Box<dyn ResultBody>>, ExecutionError> {
        let col_specs = self.prepared.get_variable_col_specs();
        let mut rows: Vec<Vec<binders::NmbrsCell<'_>>> = Vec::with_capacity(row_value_sets.len());
        for values in row_value_sets {
            rows.push(
                binders::build_row(col_specs, values)
                    .map_err(|e| op_error("bind_error", e, false))?,
            );
        }
        let row_count = rows.len();

        let mut batch = Batch::new(self.batch_type);
        batch.set_consistency(self.consistency);
        // SRD 73 aspects govern the batch itself — uniform with statements.
        self.modifiers.apply(&mut batch);
        for _ in 0..row_count {
            batch.append_statement((*self.prepared).clone());
        }

        let result = self.session.batch(&batch, rows).await.map_err(|e| {
            op_error(
                "cql_error",
                format!(
                    "batch ({row_count} rows): {}",
                    format_cql_error(&e.to_string(), &self.stmt_text),
                ),
                // Retryable = the batch's derived retry-safety (inner
                // statements, computed once at init) AND error transience.
                self.retry_safe && crate::common::cql_error_is_retryable(&e.to_string()),
            )
        })?;

        let body = ScyllaResultBody::from_query_result(result);
        // Surface a sub-batch body only when the server returned rows
        // (an LWT `[applied]` set). A plain write's `from_query_result`
        // reports `written_rows = 1` per sub-batch, but the op's real
        // rows-written count is the summed `submitted` — carried by the
        // single `write_ack` built in `execute`, not per sub-batch.
        if body.rows.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Box::new(body)))
        }
    }
}

impl OpDispenser for ScyllaBatchDispenser {
    fn canonical_kernel(&self) -> Option<&std::sync::Arc<dyn polydat::Kernel>> {
        Some(&self.canonical_kernel)
    }
    fn rows_per_op(&self) -> usize {
        self.n
    }

    /// Surface the cumulative `rows_inserted` and `_batch_writes`
    /// counters for the live status display. `rows_inserted` drives the
    /// visible rows/s chip; `_batch_writes` is INTERNAL (leading underscore)
    /// — the denominator for the derived `rows/batch` average, filtered out
    /// of the visible chip row. Mirrors the cassandra-cpp batch dispenser's
    /// counter contract.
    fn status_counters(&self) -> Vec<(&str, u64)> {
        let total = self.rows_total.load(Ordering::Relaxed);
        if total == 0 {
            return Vec::new();
        }
        let batches = self.batch_writes.load(Ordering::Relaxed);
        let mut out = vec![("rows_inserted", total)];
        if batches > 0 {
            out.push(("_batch_writes", batches));
        }
        out
    }

    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a nmbrs_runtime::adapter::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        let wires = ctx.wires;
        // SRD-22 cover-once: read exactly the reserved sub-run — the
        // executor advanced the phase cursor by this many ordinals, so
        // the op inserts `[cycle, cycle + total)` and nothing beyond.
        // `run_len` is the fixed stride `n` except at the cursor tail,
        // where the final reservation was short (the partial batch is
        // still inserted in full).
        let total = ctx.run_len.max(1);
        Box::pin(async move {
            // SRD-68 batch contract: each iteration of the batch is
            // another pull. Advance the per-fiber wire coord and
            // re-read bind values per row.
            let gather_row = |coord: u64| -> Vec<Value> {
                wires.advance(coord);
                self.bind_names
                    .iter()
                    .map(|n| wires.get(n).unwrap_or(Value::Str(String::new().into())))
                    .collect()
            };

            // Materialize all row-value sets up front so any
            // borrowed-slice NmbrsCells stay valid through each
            // `batch()` call.
            let mut all_rows: Vec<Vec<Value>> = Vec::with_capacity(total);
            for row_idx in 0..total {
                all_rows.push(gather_row(cycle + row_idx as u64));
            }

            // Fill-to-budget: accumulate rows into the current
            // sub-batch, flushing before a row that would push the
            // estimated size over `max_batch_bytes`. Always ≥1 row
            // per sub-batch; `batch: N` (= `total` here) bounds the
            // overall pull. Sum row work into one op result.
            let mut submitted: usize = 0;
            let mut last_body: Option<Box<dyn ResultBody>> = None;
            let mut cur: Vec<Vec<Value>> = Vec::new();
            let mut cur_bytes: u64 = 0;
            for values in all_rows {
                let row_bytes = size_estimator::estimate_row_size(&values);
                if let Some(budget) = self.max_batch_bytes {
                    if !cur.is_empty() && cur_bytes + row_bytes > budget {
                        if let Some(b) = self.flush_sub_batch(&cur).await? {
                            last_body = Some(b);
                        }
                        submitted += cur.len();
                        cur.clear();
                        cur_bytes = 0;
                    }
                    if cur.is_empty()
                        && row_bytes > budget
                        && !self.oversize_warned.swap(true, Ordering::Relaxed)
                    {
                        nmbrs_runtime::diag!(
                            nmbrs_runtime::observer::LogLevel::Warn,
                            "cql batch: a single row (~{row_bytes} B) exceeds \
                             max_batch_size ({budget} B); sending it as a \
                             one-row batch — the server may still reject it",
                        );
                    }
                }
                cur.push(values);
                cur_bytes += row_bytes;
            }
            if !cur.is_empty() {
                if let Some(b) = self.flush_sub_batch(&cur).await? {
                    last_body = Some(b);
                }
                submitted += cur.len();
            }

            // Mirror nmbrs batch dispenser's `rows_inserted`
            // capture — drives the `rows/s` status metric. Lands on
            // the per-fiber kernel via ctx.wires.write.
            let _ = ctx
                .wires
                .write("rows_inserted", polydat::ast::Value::U64(submitted as u64));
            // Aggregate the same values onto the dispenser's status
            // counters so the display sees them through
            // `status_counters()`. Reached only on the success path (a
            // failed sub-batch returns early above), so `batch_writes`
            // counts exactly the ops that wrote rows — the denominator
            // for the true average batch size.
            self.rows_total
                .fetch_add(submitted as u64, Ordering::Relaxed);
            if submitted >= 1 {
                self.batch_writes.fetch_add(1, Ordering::Relaxed);
            }
            // A CQL write reports the rows it wrote as its result: the
            // batch's `element_count` is `submitted` (rows written),
            // just as a SELECT's is the rows it returned. An LWT batch
            // that returned an `[applied]` row surfaces that row-set
            // instead (returned rows win — see `element_count`).
            let body: Option<Box<dyn ResultBody>> = match last_body {
                Some(rows_body) => Some(rows_body),
                None => Some(Box::new(ScyllaResultBody::write_ack(submitted as u64))),
            };
            Ok(OpResult {
                body,
                skipped: false,
            })
        })
    }
}
