// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Cascade-extern emission helpers — building blocks the scope
//! builders use to push individual lines of Polydat source into a
//! string that's later folded into a [`polydat::kernel::subcontext::PolydatMatter`]
//! and handed to the builder.
//!
//! These are the "build" half of walk + build + synthesize: the
//! walker has decided that a particular name should be cascaded;
//! these helpers do the per-name source-line emission consistent
//! with nmbrs-runtime's Polydat conventions (chain-aware lookup, type
//! widening rules, escape handling).

use std::collections::HashSet;

use crate::scope_kernel::ScopeKernel;

use super::helpers::{format_workload_param_as_polydat_literal, value_to_param_string};

/// Emit `const NAME := <literal>` for one workload param into
/// `source`, choosing the literal value via the canonical
/// chain-aware lookup order:
///
/// 1. `parent_kernel.lookup(name)` — if `Some`, this is the
///    chain-resolved value. A `bindings:` / `set:` scope above
///    us may have shadowed the workload-param default; whichever
///    `const NAME := <override>` is closest in the chain wins
///    via the local-final transit-suppression rule
///    (SRD-13f §"Local-authoritative shadow"). If the lookup is
///    non-scalar (vectors etc.), skip the chain path and fall
///    back.
/// 2. `hashmap_default` — the workload-load-time default from
///    `params:` plus CLI overrides. Used when nothing in the
///    chain has a value for `name`.
///
/// Pre-marks `name` in `emitted` so subsequent cascade passes in
/// the caller skip it. Optionally appends to `inherited_names`
/// for the inherited-output bookkeeping consumers also do.
///
/// **Why this helper exists**: pre-helper, seven distinct
/// synthesizer sites each rolled their own `const NAME := <hashmap>`
/// emission, reading the HashMap directly without consulting the
/// chain. Any name a scenario-tree `set:` or `bindings:` scope
/// shadowed kept the stale HashMap default inside any sub-scope
/// synthesized via one of those sites — breaking the SRD-21 /
/// SRD-18 contract that the kernel chain is the single
/// resolution surface. Routing through this helper closes that
/// hole categorically.
pub fn emit_workload_param_chain_aware(
    name: &str,
    hashmap_default: &str,
    parent_kernel: &ScopeKernel,
    source: &mut String,
    emitted: &mut HashSet<String>,
    inherited_names: Option<&mut Vec<String>>,
) {
    if emitted.contains(name) {
        return;
    }
    let value_str = parent_kernel
        .lookup(name)
        .and_then(|v| value_to_param_string(&v))
        .unwrap_or_else(|| hashmap_default.to_string());
    let literal = format_workload_param_as_polydat_literal(&value_str);
    source.push_str(&format!("const {name} := {literal}\n"));
    emitted.insert(name.to_string());
    if let Some(inh) = inherited_names {
        inh.push(name.to_string());
    }
}
