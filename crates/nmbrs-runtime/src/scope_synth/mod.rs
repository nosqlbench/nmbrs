// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Scope-construction substrate for nmbrs-runtime.
//!
//! Module name disambiguates from the pre-existing
//! [`crate::synthesis`] (which handles fiber/op synthesis). Here
//! we handle **scope** synthesis — turning workload-shape intent
//! (a comprehension, a do-loop, a phase, an op template) into a
//! child [`polydat::kernel::PolydatKernel`].
//!
//! This module hosts the walking + Surface #7 helpers + scope-
//! builder functions that drive [`polydat::kernel::subcontext::SubcontextBuilder`]
//! to materialize new scope kernels. See
//! `polydat/docs/design/comprehension_cutover_contact_surfaces.md`
//! for the architectural framing.
//!
//! ## What's here
//!
//! - [`helpers`] — Surface #7: pure utilities for turning typed
//!   values, workload-param strings, and PortType variants into
//!   Polydat source literals / extern type names. Also the placeholder
//!   scanner used by every scope-builder walk.
//!
//! ## What's planned (PR 9c-1b in progress)
//!
//! - `cascade` — the shared cascade-extern walker that the four
//!   sister scope builders (`build_phase_scope_kernel`,
//!   `build_do_loop_scope_kernel`, `build_op_template_scope_kernel`,
//!   and the forthcoming `build_for_each_scope_kernel`) will
//!   consume in lieu of each reinventing the same walk.
//! - `for_each` — the comprehension-specific iter-var emission +
//!   shared cascade walker invocation that replaces
//!   `polydat::iteration::comprehension::synthesize_for_each_scope`.
//!
//! ## What lives in `scope.rs` for now
//!
//! `build_phase_scope_kernel`, `build_do_loop_scope_kernel`,
//! `build_op_template_scope_kernel`. These will migrate into this
//! module (and refactor onto the shared cascade walker) as part of
//! the same push.

pub mod cascade;
pub mod cascade_emit;
pub mod for_each;
pub mod helpers;

pub use cascade::{CascadeInputs, CascadeOutputs, cascade_parent_into_source};
pub use cascade_emit::emit_workload_param_chain_aware;
pub use for_each::build_for_each_scope_kernel;
pub use helpers::{
    collect_leaf_placeholders, format_value_as_final_literal, format_value_as_polydat_literal,
    format_workload_param_as_polydat_literal, port_type_to_extern_name, scan_one,
    value_to_param_string, workload_param_type_name,
};
