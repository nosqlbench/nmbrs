// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! # Scylla engine
//!
//! Pure-Rust CQL engine backed by the [scylla 1.6](https://crates.io/crates/scylla)
//! driver. Speaks the Apache Cassandra wire protocol — works
//! against both Apache Cassandra and ScyllaDB.
//!
//! `scylla` is the internal driver name — `adapter=cql` is the
//! only user-facing adapter; `cqldriver=scylla` selects this
//! driver from inside that adapter. The `DriverImpl` below
//! carries the factory and the driver-specific known params;
//! there is no separate `AdapterRegistration` because `scylla`
//! is never an adapter name on its own.

mod batch;
mod binder_meta;
mod binders;
mod op_modifier;
mod prepared;
mod raw;
mod result;
mod settings;

use std::sync::Arc;

use crate::common::{CqlConfig, CqlConsistency, OpMode, STMT_FIELD_NAMES};
use nmbrs_runtime::adapter::{
    AdapterError, DriverAdapter, DriverImpl, ExecutionError, OpDispenser, StatusMetric,
};
use nmbrs_workload::model::ParsedOp;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::statement::Consistency;

use result::ScyllaResultBody;

/// Bridge: [`crate::common::CqlConsistency`] →
/// `scylla::statement::Consistency`. Each engine keeps its own
/// driver-specific consistency type; the shared enum stays
/// driver-agnostic.
fn to_scylla_consistency(c: CqlConsistency) -> Consistency {
    match c {
        CqlConsistency::Any => Consistency::Any,
        CqlConsistency::One => Consistency::One,
        CqlConsistency::Two => Consistency::Two,
        CqlConsistency::Three => Consistency::Three,
        CqlConsistency::Quorum => Consistency::Quorum,
        CqlConsistency::All => Consistency::All,
        CqlConsistency::LocalQuorum => Consistency::LocalQuorum,
        CqlConsistency::EachQuorum => Consistency::EachQuorum,
        CqlConsistency::LocalOne => Consistency::LocalOne,
    }
}

/// CQL adapter using the scylla pure-Rust driver.
pub struct ScyllaCqlAdapter {
    session: Arc<Session>,
    /// The parsed connection config this adapter was built from — retained so
    /// `map_op` can derive the phase's own SRD-35 fingerprint
    /// (`config.to_resource_key("scylla").render_key()`) and bind it as the
    /// `cql_session_key` scope constant for GK-resolved `max_batch_size`
    /// (SRD-103 §3–4). Matches the pool entry's key, so the resource lookup hits.
    config: CqlConfig,
    consistency: Consistency,
}

impl ScyllaCqlAdapter {
    pub async fn connect(config: &CqlConfig) -> Result<Self, String> {
        let consistency = to_scylla_consistency(config.consistency);

        let mut builder = SessionBuilder::new();
        for host in config
            .hosts
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            // Hosts may be `host:port`; if not, append the configured port.
            if host.contains(':') {
                builder = builder.known_node(host);
            } else {
                builder = builder.known_node(format!("{host}:{}", config.port));
            }
        }
        if let (Some(u), Some(p)) = (&config.username, &config.password) {
            builder = builder.user(u, p);
        }
        // Connection ESTABLISHMENT timeout (was conflated with the per-request
        // timeout before `connect_timeout` existed).
        builder =
            builder.connection_timeout(std::time::Duration::from_millis(config.connect_timeout_ms));
        // Connect-survival knobs (catchup D2): map to the driver's
        // CQL-layer keepalives — `heartbeat_interval` paces the
        // keepalive requests, `connection_idle_timeout` bounds how
        // long an unanswered keepalive is tolerated before the
        // connection is declared dead. Same survive-the-stall
        // intent as cassandra-cpp's heartbeat/idle machinery.
        builder = builder.keepalive_interval(std::time::Duration::from_millis(
            config.heartbeat_interval_ms,
        ));
        builder = builder.keepalive_timeout(std::time::Duration::from_millis(
            config.connection_idle_timeout_ms,
        ));
        // scylla manages reconnection internally — the `reconnect_*`
        // knobs have no equivalent. Accepting them silently would be
        // a lie; say so once at connect.
        if config.reconnect_params_explicit {
            nmbrs_runtime::diag!(
                nmbrs_runtime::observer::LogLevel::Warn,
                "cql(scylla): reconnect_base_delay / reconnect_max_delay have \
                 no scylla-driver equivalent and are ignored — the driver \
                 manages reconnection internally"
            );
        }
        if !config.keyspace.is_empty() {
            builder = builder.use_keyspace(config.keyspace.clone(), false);
        }

        let session = builder
            .build()
            .await
            .map_err(|e| format!("scylla connect: {e}"))?;
        Ok(Self {
            session: Arc::new(session),
            config: config.clone(),
            consistency,
        })
    }
}

impl DriverAdapter for ScyllaCqlAdapter {
    // The user-facing adapter is `cql`, regardless of which
    // engine backs it. `scylla` is an internal driver choice
    // selected via `cqldriver=`; it never appears in the
    // adapter-lookup table or in op-level `adapter: …` fields.
    fn name(&self) -> &str {
        "cql"
    }

    fn default_status_metrics(&self) -> Vec<StatusMetric> {
        crate::common::default_status_metrics()
    }

    /// SRD-103/104 — publish a [`CqlSessionHandle`](crate::common::CqlSessionHandle)
    /// over this adapter's connected session as the pool-entry accessor
    /// payload. The settings source runs on the SAME `Arc<Session>` the op
    /// path uses.
    fn accessor_payload(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        let source = settings::ScyllaSettingsSource::new(self.session.clone());
        Some(Arc::new(crate::common::CqlSessionHandle::new(
            "scylla",
            Arc::new(source),
        )))
    }

    fn map_op<'a>(
        &'a self,
        template: &'a ParsedOp,
        parent: std::sync::Arc<dyn polydat::Kernel>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Box<dyn OpDispenser>, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            let (stmt_text, stmt_field) = STMT_FIELD_NAMES
                .iter()
                .find_map(|key| -> Option<(String, &'static str)> {
                    let v = template.op.get(*key)?;
                    Some((v.as_str()?.to_string(), *key))
                })
                .ok_or_else(|| {
                    "CQL op requires a 'raw:', 'simple:', 'prepared:', or 'stmt:' field".to_string()
                })?;

            // Replace bind points with `?` markers for prepared mode
            // and capture the bind-point names in `?` order so we can
            // look up typed values by name from `ResolvedFields`. The
            // runtime puts every op field (including the stmt text
            // itself) into `fields.values`; only the named bind points
            // belong on the wire, in the order they appeared in the
            // statement text.
            let bind_names = nmbrs_workload::bindpoints::referenced_bindings(&stmt_text);
            let prepared_text =
                nmbrs_workload::bindpoints::replace_bind_points_with_markers(&stmt_text);
            // Workload-author lvalue assertions per bind-point: a
            // `{name:*}` or `{name:<polydat-type>}` suffix in the
            // statement text overrides the cluster-side parameter
            // type for binder verification (and only for verification;
            // the bind path still uses cluster metadata). Indices
            // line up with `bind_names` because both come from
            // walking the same statement text in the same order.
            let lvalue_specs: Vec<Option<nmbrs_workload::bindpoints::LvalueSpec>> = {
                use nmbrs_workload::bindpoints::{BindPoint, extract_bind_points};
                extract_bind_points(&stmt_text)
                    .into_iter()
                    .filter_map(|bp| match bp {
                        BindPoint::Reference { lvalue_spec, .. } => Some(lvalue_spec),
                        BindPoint::InlineDefinition(_) => None,
                    })
                    .collect()
            };

            // A `batch:` row cap OR a `max_batch_size:` byte budget both
            // select the batch executor (SRD-103 §6): `max_batch_size`
            // alone byte-bounds a dynamically-sized batch.
            let has_batch = template.params.contains_key("batch")
                || template.params.contains_key("max_batch_size");
            let mode = OpMode::from_stmt_field(stmt_field, has_batch);

            // SRD 73 — build the per-op modifier chain at dispenser
            // initializer time. The chain captures any universal CQL
            // field the user bound in the Polydat scope (workload params,
            // scenario-tree `set:` shadows, op-template fields); names
            // the scope doesn't bind contribute nothing. Critical-path
            // execute() just calls `chain.apply(&mut stmt)` — no
            // re-evaluation.
            let op_label = template.name.clone();
            let modifiers_for_raw = crate::common::op_modifier::build_cql_modifier_chain::<
                op_modifier::ScyllaModifierFactory<scylla::statement::Statement>,
            >(&*parent, op_label.clone())?;
            let modifiers_for_prepared = crate::common::op_modifier::build_cql_modifier_chain::<
                op_modifier::ScyllaModifierFactory<scylla::statement::prepared::PreparedStatement>,
            >(&*parent, op_label)?;

            match mode {
                OpMode::Raw => Ok(Box::new(raw::ScyllaRawDispenser::new(
                    parent.clone(),
                    self.session.clone(),
                    self.consistency,
                    stmt_text,
                    modifiers_for_raw,
                )) as Box<dyn OpDispenser>),
                OpMode::Prepared | OpMode::Batch => {
                    // Prepare against the cluster and verify the binder
                    // against the dispenser's parent kernel — the per-op
                    // dispenser-init compulsion. Both prepared-mode and
                    // batch-mode use the same inner prepared statement,
                    // so they share this preparation step.
                    let mut prep =
                        self.session
                            .prepare(prepared_text.clone())
                            .await
                            .map_err(|e| {
                                format!("scylla prepare '{}': {e}", truncate_stmt(&prepared_text),)
                            })?;
                    prep.set_consistency(self.consistency);
                    // SRD 73: layer the per-op universal-field overrides
                    // before Arc-wrapping (PreparedStatement mutation is
                    // single-owner before sharing).
                    modifiers_for_prepared.apply(&mut prep);

                    // Build the typed binder from the prepared statement's
                    // variable col-specs. Wire references come from the
                    // `bind_names` list (one per `?` placeholder, in order).
                    //
                    // Any slot whose CQL type doesn't have a precise
                    // polydat mapping yet falls back to `Str` and gets
                    // a WARN so the operator can see exactly which
                    // positions lost typed verification — surfacing
                    // the long-tail CQL types that need a precise
                    // mapping added in `binder_meta::cql_to_polydat`.
                    // Build the binder slots. For each `?`-position:
                    //
                    //   1. Resolve the workload-author lvalue
                    //      assertion (`{name:*}` / `{name:<type>}`)
                    //      from the parsed bind-points. If present,
                    //      it overrides the cluster-side type for
                    //      verification — this is the per-bindpoint
                    //      opt-in for type fusion (`:*` → Str-lvalue,
                    //      anything goes; `:<type>` → asserted type
                    //      from `PortType::from_workload_name`).
                    //   2. Otherwise fall back to the cluster-side
                    //      type via `binder_meta::cql_to_polydat`. Any
                    //      cluster type lacking a precise polydat
                    //      mapping warns (per-slot) so the operator
                    //      can see which slots are un-typed.
                    let col_specs = prep.get_variable_col_specs();
                    let mut slot_build_err: Option<String> = None;
                    let slots: Vec<polydat::binder::BinderSlot> = col_specs.iter()
                    .zip(bind_names.iter())
                    .enumerate()
                    .map(|(idx, (cluster_spec, name))| {
                        use nmbrs_workload::bindpoints::LvalueSpec;
                        // Compute the cluster-side (precise or
                        // Str-fallback) polydat type — used in all
                        // arms below as the slot's truthful lvalue
                        // type, whether or not the workload's
                        // opt-in overrides it.
                        let (cluster_lvalue, fallback) =
                            binder_meta::cql_to_polydat(cluster_spec.typ());
                        let (lvalue_type, allow_fusion) =
                            match lvalue_specs.get(idx).and_then(|s| s.as_ref()) {
                                Some(LvalueSpec::Wildcard) => {
                                    nmbrs_runtime::diag!(
                                        nmbrs_runtime::observer::LogLevel::Info,
                                        "scylla op '{op}' field '{field}' slot [{idx}] wire `{name}`: \
                                         `:*` wildcard opt-in seen on this bind-point — polydat \
                                         binder slot keeps cluster-reported lvalue type \
                                         `{cluster_lvalue}` with allow_fusion=true; verifier \
                                         skips the strict rvalue→lvalue rule for this slot.",
                                        op = template.name,
                                        field = stmt_field,
                                    );
                                    (cluster_lvalue, true)
                                }
                                Some(LvalueSpec::Explicit(type_name)) => {
                                    match polydat::ast::PortType::from_workload_name(type_name) {
                                        Some(pt) => {
                                            nmbrs_runtime::diag!(
                                                nmbrs_runtime::observer::LogLevel::Info,
                                                "scylla op '{op}' field '{field}' slot [{idx}] wire `{name}`: \
                                                 `:{type_name}` lvalue assertion seen on this bind-point \
                                                 — using workload-asserted polydat type `{type_name}` \
                                                 (cluster reports `{cluster_lvalue}`).",
                                                op = template.name,
                                                field = stmt_field,
                                            );
                                            (pt, false)
                                        }
                                        None => {
                                            slot_build_err = Some(format!(
                                                "scylla op '{op}' field '{field}' slot [{idx}] wire `{name}`: \
                                                 unknown polydat type name `{type_name}` in lvalue spec \
                                                 `:{type_name}`. Accepted names: u64, f64, u32, i32, \
                                                 i64, f32, bool, str, bytes, json, vec_f32, vec_i32.",
                                                op = template.name,
                                                field = stmt_field,
                                            ));
                                            (cluster_lvalue, false)
                                        }
                                    }
                                }
                                None => {
                                    if let Some(cql_label) = fallback {
                                        nmbrs_runtime::diag!(
                                            nmbrs_runtime::observer::LogLevel::Warn,
                                            "scylla op '{op}' field '{field}' slot [{idx}] wire `{name}`: \
                                             CQL type {cql_label} has no precise polydat mapping yet — \
                                             falling back to Str-lvalue, which permits any rvalue and \
                                             effectively bypasses typed verification for this slot. \
                                             Add a precise arm to \
                                             crates/nmbrs-adapter-cql/src/scylla/binder_meta.rs::cql_to_polydat \
                                             when this type becomes a verification bottleneck, or \
                                             silence this warning intentionally by spelling the \
                                             bind-point with the `:*` wildcard suffix (i.e. write \
                                             `{{{name}:*}}` in place of `{{{name}}}` in the \
                                             workload template).",
                                            op = template.name,
                                            field = stmt_field,
                                        );
                                    }
                                    // Non-strict default: auto-permit fusion for
                                    // text-natural lvalues. The CQL text/varchar/ascii
                                    // protocol slot will text-coerce any rvalue at
                                    // bind time, so polydat doesn't need to enforce a
                                    // strict rvalue→Str match. Adapter signals this
                                    // explicitly via `allow_fusion: true` (was an
                                    // implicit `is_text_natural` rule in polydat
                                    // before; the signal is now on the slot).
                                    //
                                    // The CUSTOM-fallback case (above) already
                                    // mapped to Str; this arm picks up both that
                                    // case and the precisely-mapped Text/Ascii/Varchar
                                    // columns. A future `strict=true` adapter param
                                    // would skip this auto-permit.
                                    let allow_fusion = matches!(
                                        cluster_lvalue, polydat::ast::PortType::Str);
                                    (cluster_lvalue, allow_fusion)
                                }
                            };
                        polydat::binder::BinderSlot {
                            wire: name.clone(),
                            lvalue_type,
                            allow_fusion,
                        }
                    })
                    .collect();
                    if let Some(msg) = slot_build_err {
                        return Err(msg);
                    }
                    if !slots.is_empty() {
                        let binder = polydat::binder::Binder::Positional {
                            field: stmt_field.to_string(),
                            slots,
                        };
                        nmbrs_runtime::adapter::verify_binders(&[binder], &*parent)?;
                    }
                    let prepared_arc = std::sync::Arc::new(prep);

                    match mode {
                        OpMode::Prepared => Ok(Box::new(prepared::ScyllaPreparedDispenser::new(
                            parent.clone(),
                            self.session.clone(),
                            prepared_arc,
                            prepared_text,
                            bind_names,
                        )) as Box<dyn OpDispenser>),
                        OpMode::Batch => {
                            let batch_type_name = template
                                .params
                                .get("batchtype")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_lowercase())
                                .unwrap_or_default();
                            let batch_type = match batch_type_name.as_str() {
                                "logged" => scylla::statement::batch::BatchType::Logged,
                                "counter" => scylla::statement::batch::BatchType::Counter,
                                _ => scylla::statement::batch::BatchType::Unlogged,
                            };
                            // A batch DERIVES its retry-safety from its inner
                            // statements — uniform template with stride, computed
                            // ONCE here, never per stanza: counter batches and LWT
                            // statements are not retry-safe; plain PK-keyed
                            // upserts are.
                            let batch_retry_safe = batch_type_name != "counter"
                                && crate::common::cql_statement_retry_safe(&prepared_text);
                            // SRD-103 §3 — both `batch:` (cursor stride / row cap)
                            // and `max_batch_size:` (byte budget) are GK-resolved op
                            // fields. A literal (`batch: 8`, `64KB`) resolves
                            // directly; an expression referencing the CQL session
                            // nodes is evaluated against a subscope with this
                            // phase's own `cql_session_key` bound, after the
                            // referenced settings are pre-read into the session memo.
                            let session_key = self.config.to_resource_key("scylla").render_key();
                            // Workload-authored `batch:` cursor stride (0 = unset →
                            // the byte budget, if any, drives the row count). Used
                            // directly, unfloored downstream. `batch-size` is the
                            // accepted alias key.
                            let batch_n: usize =
                                crate::common::session_handle::resolve_batch_count(
                                    &*parent,
                                    &session_key,
                                    template
                                        .params
                                        .get("batch")
                                        .or_else(|| template.params.get("batch-size")),
                                )
                                .await
                                .map_err(|e| format!("op '{}': {e}", template.name))?
                                .unwrap_or(0);
                            let max_batch_bytes =
                                crate::common::session_handle::resolve_max_batch_bytes(
                                    &*parent,
                                    &session_key,
                                    template.params.get("max_batch_size"),
                                )
                                .await
                                .map_err(|e| format!("op '{}': {e}", template.name))?;
                            // SRD-22 cover-once — settle the FIXED uniform stride N
                            // now (not per-execute). Only characterize a row when a
                            // byte budget must be converted to a row count; `batch:N`
                            // / single-row need no probe.
                            let row_size = if max_batch_bytes.is_some() {
                                crate::common::size_estimator::characterize_row_size(
                                    &*parent,
                                    &bind_names,
                                )
                            } else {
                                0
                            };
                            let stride_n = crate::common::size_estimator::fixed_batch_stride(
                                row_size,
                                batch_n,
                                max_batch_bytes,
                            );
                            // A batch is a statement too — resolve the SAME universal
                            // fields into a batch-targeted chain so consistency /
                            // serial / request timeout / tracing all reach the batch
                            // itself, uniform with the single-statement path.
                            let modifiers_for_batch =
                                crate::common::op_modifier::build_cql_modifier_chain::<
                                    op_modifier::ScyllaModifierFactory<
                                        scylla::statement::batch::Batch,
                                    >,
                                >(&*parent, template.name.clone())?;
                            Ok(Box::new(batch::ScyllaBatchDispenser::new(
                                parent.clone(),
                                self.session.clone(),
                                prepared_arc,
                                prepared_text,
                                bind_names,
                                stride_n,
                                max_batch_bytes,
                                batch_type,
                                self.consistency,
                                modifiers_for_batch,
                                batch_retry_safe,
                            )) as Box<dyn OpDispenser>)
                        }
                        OpMode::Raw => unreachable!(
                            "Raw arm handled above; the prepare/batch combined arm \
                         is reached only when mode is Prepared or Batch."
                        ),
                    }
                }
            }
        })
    }

    fn known_op_params(&self) -> &'static [&'static str] {
        // SRD 73: universal per-op field surface, in addition to
        // existing CQL adapter params.
        crate::common::op_modifier::CQL_UNIVERSAL_FIELDS
    }
}

// =========================================================================
// Inventory registration
// =========================================================================

// Register `scylla` as a driver implementation of the `cql`
// adapter. Higher rank than cassandra-cpp (100) so binaries
// that link both default to cassandra-cpp; flip with
// `cqldriver=scylla`.
inventory::submit! {
    DriverImpl {
        adapter: "cql",
        driver: "scylla",
        default_rank: 200,
        create: |params| Box::pin(async move {
            let config = CqlConfig::from_params(&params)
                .map_err(|e| format!("scylla config error: {e}"))?;
            ScyllaCqlAdapter::connect(&config).await
                .map(|a| Arc::new(a) as Arc<dyn DriverAdapter>)
                .map_err(|e| format!("scylla connection failed: {e}"))
        }),
        known_params: || &[
            "hosts", "host", "port", "keyspace", "connect_keyspace", "consistency",
            "username", "password", "timeout", "request_timeout_ms",
            "connect_timeout", "request_timeout",
            // Exponential-reconnect knobs + tracing: accepted for parity
            // with the cassandra-cpp engine so a `cqldriver=` switch doesn't
            // trip the unknown-param guard, but inert on the scylla engine
            // (it manages reconnection internally and doesn't yet honor
            // per-statement tracing) — declared until wired.
            "reconnect_base_delay", "reconnect_max_delay",
            "heartbeat_interval", "connection_idle_timeout",
            "trace_rate", "trace_log",
        ],
    }
}

// SRD-35 Push B: scylla engine declares itself
// pool-shareable. `scylla::Session` is documented Send +
// Sync and designed for concurrent use across many
// clients. Phases whose params produce equal
// `CqlConfig::to_resource_key("scylla")` keys share a
// single `ScyllaCqlAdapter` for the whole workload.
inventory::submit! {
    nmbrs_runtime::adapter::SharedDriverRegistration {
        adapter: "cql",
        driver: "scylla",
        share_capability: nmbrs_runtime::resource_pool::ShareCapability::Shared,
        resource_key: |params| {
            let cfg = crate::common::CqlConfig::from_params(params)
                .map_err(|e| format!("scylla config error: {e}"))?;
            Ok(cfg.to_resource_key("scylla"))
        },
    }
}

// =========================================================================
// Helpers shared across dispenser modules
// =========================================================================

pub(super) fn op_error(
    error_name: &str,
    msg: impl Into<String>,
    retryable: bool,
) -> ExecutionError {
    ExecutionError::Op(AdapterError {
        error_name: error_name.into(),
        message: msg.into(),
        retryable,
    })
}

pub(super) fn truncate_stmt(text: &str) -> String {
    if text.len() > 200 {
        format!("{}...", &text[..200])
    } else {
        text.to_string()
    }
}

/// Render a CQL execution error in a rustc-like format with the
/// offending statement and a caret at the reported position.
///
/// The driver returns errors like `"... line 1:31 no viable
/// alternative at character '_'"`. When a `line N:M` (1-based)
/// position is present, we extract it, find the matching line
/// in `stmt`, and underline the column with a caret. Otherwise
/// we fall back to a single-line `error\n  statement: …` form
/// that's still readable.
///
/// Example:
///
/// ```text
/// error: cql syntax: no viable alternative at character '_'
///   --> line 1, column 31
///    |
///  1 | DROP INDEX IF EXISTS baselines._meta_idx
///    |                               ^
/// ```
pub(super) fn format_cql_error(err: &str, stmt: &str) -> String {
    let err_str = err.to_string();
    let (line_no, col_no, message) = match parse_line_col(&err_str) {
        Some(p) => p,
        None => {
            return format!("cql error: {err_str}\n  statement: {}", truncate_stmt(stmt));
        }
    };

    let lines: Vec<&str> = stmt.lines().collect();
    if lines.is_empty() || line_no == 0 || line_no > lines.len() {
        return format!("cql error: {err_str}\n  statement: {}", truncate_stmt(stmt));
    }

    let target_line = lines[line_no - 1];
    let line_num_str = line_no.to_string();
    let gutter_w = line_num_str.len();
    // The content line is rendered as
    // ` <line_num> | <text>`. The blank-gutter lines (`-->`,
    // the divider above the content, and the caret line below
    // it) need their `|` at the same column. That column is
    // `1 + gutter_w + 1` (leading indent + width of the line
    // number + the space before `|`). `gutter_pad` is the
    // padding that puts `|` at that column on the no-line-
    // number rows.
    let gutter_pad = " ".repeat(1 + gutter_w + 1);

    // Caret column. The driver reports 1-based char positions;
    // anything else gets clamped into range so we still show
    // the line.
    let caret_col = col_no.saturating_sub(1).min(target_line.chars().count());
    let caret_pad = " ".repeat(caret_col);

    let mut out = String::new();
    out.push_str(&format!("cql syntax: {message}\n"));
    out.push_str(&format!(
        "{gutter_pad}--> line {line_no}, column {col_no}\n"
    ));
    out.push_str(&format!("{gutter_pad}|\n"));
    out.push_str(&format!(" {line_num_str} | {target_line}\n"));
    out.push_str(&format!("{gutter_pad}| {caret_pad}^"));
    out
}

/// Pull `line N:M` from an error string (1-based) and return
/// `(line, col, trailing_message)`. The trailing message is the
/// substring after the position, with a leading "no viable
/// alternative…"-style descriptor when the driver provides one.
fn parse_line_col(err: &str) -> Option<(usize, usize, String)> {
    // Look for the `line N:M` shape anywhere in the error
    // string. Cassandra's wire-protocol error variants embed it
    // verbatim regardless of which preamble the driver wraps it
    // in.
    let bytes = err.as_bytes();
    let needle = b"line ";
    let start = (0..bytes.len().saturating_sub(needle.len()))
        .find(|&i| &bytes[i..i + needle.len()] == needle)?;
    let after = &err[start + needle.len()..];

    let (line_str, rest) = after.split_once(':')?;
    let line: usize = line_str.trim().parse().ok()?;

    // Column: digits up to the next non-digit.
    let col_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let col: usize = rest[..col_end].parse().ok()?;

    let mut message = rest[col_end..].trim_start().to_string();
    if message.is_empty() {
        // Fall back to the whole error string if there's no
        // trailing descriptor — the position alone is the
        // signal.
        message = err.to_string();
    }
    Some((line, col, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_line_col_with_message() {
        let err = "Database returned an error: The submitted query has a syntax error, \
                   Error message: line 1:31 no viable alternative at character '_'";
        let (l, c, m) = parse_line_col(err).expect("should parse");
        assert_eq!(l, 1);
        assert_eq!(c, 31);
        assert!(m.starts_with("no viable alternative"));
    }

    #[test]
    fn parses_line_col_multiline() {
        let err = "syntax error: line 3:7 mismatched input";
        let (l, c, _) = parse_line_col(err).expect("should parse");
        assert_eq!(l, 3);
        assert_eq!(c, 7);
    }

    #[test]
    fn returns_none_when_no_position() {
        assert!(parse_line_col("connection refused").is_none());
    }

    #[test]
    fn renders_with_caret() {
        let stmt = "DROP INDEX IF EXISTS baselines._meta_idx";
        let err = "Database returned an error: line 1:31 no viable alternative at character '_'";
        let out = format_cql_error(err, stmt);
        // Header
        assert!(
            out.starts_with("cql syntax: no viable alternative"),
            "got:\n{out}"
        );
        // Statement appears
        assert!(
            out.contains("DROP INDEX IF EXISTS baselines._meta_idx"),
            "got:\n{out}"
        );
        // Caret line ends with `^`
        assert!(out.trim_end().ends_with('^'), "got:\n{out}");
    }

    #[test]
    fn falls_back_when_no_line_col() {
        let stmt = "SELECT 1";
        let err = "no host available";
        let out = format_cql_error(err, stmt);
        assert!(out.starts_with("cql error: no host available"));
        assert!(out.contains("SELECT 1"));
    }
}
