// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Contract & axioms: [SRD 08](../../docs/SRD/08_metricsql.md).
//!
//! MetricsQL parser + evaluator. Rust port of
//! [VictoriaMetrics/metricsql](https://github.com/VictoriaMetrics/metricsql)
//! plus the relevant chunks of `vmselect/promql` for query
//! evaluation.
//!
//! ## Goals
//!
//! - **Parser parity**: every test case in upstream's
//!   `parser_test.go` and `lexer_test.go` round-trips through
//!   the Rust parser → AST → prettifier with the same output
//!   the Go implementation produces.
//! - **Pluggable data source**: the evaluator never touches
//!   storage directly; consumers implement [`DataSource`] to
//!   feed time series into the engine. nmbrs's `metrics.db`
//!   reader is one such implementation.
//! - **Subset evaluator**: the function table starts with the
//!   selectors / aggregations / binary-ops / common rollups
//!   that nmbrs's plot/table specs need. Less-common functions
//!   land as queries demand them.
//!
//! ## Status
//!
//! Foundation phase. Lexer + parser + AST in flight. The
//! evaluator is a stub that returns empty results.
//!
//! ## Module layout
//!
//! - [`lexer`] — token stream over a query string
//! - [`ast`]   — query AST nodes
//! - [`parser`]— token stream → AST
//! - [`prettifier`] — AST → canonical query string (used by
//!   the round-trip parity tests)
//! - [`eval`]  — AST → query plan → result, against a
//!   [`DataSource`]
//!
//! ## See also
//!
//! - Upstream Go: <https://github.com/VictoriaMetrics/metricsql>
//! - Linked in this repo at `links/metricsql/`
//! - Test fixtures: `tests/fixtures/*.json`, harvested from
//!   the upstream `_test.go` files via
//!   `scripts/extract_fixtures.go`.

// nmbrs-metricsql is a standalone, extractable library (a VictoriaMetrics
// MetricsQL port). Its public API is its OWN library contract — the parser,
// AST, evaluator, streaming reducers, OpenMetrics types, and the
// `MetricCatalog` trait (SRD-49) — not merely "what the nmbrs workspace
// imports". So these modules stay `pub`; the SRD-05 D5 narrowing applies to
// workspace-internal crates, not to standalone libraries.
pub mod ast;
pub mod eval;
pub mod grammar;
pub mod lexer;
pub mod parser;
pub mod prettifier;
pub mod query_rewrite;
#[cfg(feature = "runtime")]
pub mod runtime;
pub mod streaming;
// SRD-86 — the `metricsql_*` polydat node family + the Vector→Value
// projector. Behind `polydat-nodes` so the engine stays polydat-free
// for parse/evaluate-only consumers.
#[cfg(feature = "polydat-nodes")]
pub mod polydat_nodes;

pub use ast::Expr;
pub use eval::{DataSourceError, EvalContext, EvalError, MetricAccess, evaluate, evaluate_range};
pub use parser::{ParseError, parse, parse_for_prettify};
pub use streaming::{CompileError, StreamingPlan, compile_streaming};
