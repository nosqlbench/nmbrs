// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The engine nmbrs runs its kernels on.
//!
//! Scope synthesis reads program structure on the interpreter
//! (`PolydatProgram`), and pull plans resolve their indices against the
//! interpreter's program. Every kernel that runs — each scope of the
//! tree ([`crate::scope_kernel::ScopeKernel`]), each fiber's main kernel,
//! its per-op kernels, the kernels adapters hold — runs on a compiled
//! engine instead, bound under its parent through the engine-neutral
//! `Kernel` surface (polydat's native_scope_trees.md).
//!
//! A compiled image stands in for an interpreter program only where it
//! reports the same inputs and outputs in the same order, which is what
//! lets an index resolved on the interpreter's program drive the image.
//! polydat guarantees that for a scope module's images; an image
//! compiled separately from the same source is checked here, once, and
//! an image that disagrees is not used.

use std::sync::Arc;

use polydat::kernel::subcontext::{Child, RootMarker, ScopeModule};
use polydat::kernel::{KernelProgram, PolydatProgram};

/// The module an op-template scope finalizes to: its interpreter
/// program plus the settings and write-throughs every other engine's
/// image is built from.
pub type OpTemplateModule = ScopeModule<Child<RootMarker>>;

/// The engine scope, fiber and per-op kernels run on: polydat's default, the
/// most native form the build has.
pub fn fiber_engine() -> polydat::Engine {
    polydat::Engine::default()
}

/// Whether `image` has `program`'s inputs and outputs in the same
/// order, coordinates first — the condition for driving it with indices
/// resolved on `program`.
pub fn agrees(program: &PolydatProgram, image: &Arc<dyn KernelProgram>) -> bool {
    let kernel = image.clone().create_kernel();
    let outputs: Vec<String> = program
        .output_names()
        .iter()
        .map(|n| n.to_string())
        .collect();
    kernel.input_names() == program.input_names()
        && kernel.output_names() == outputs
        && kernel.coord_count() == program.coord_count()
}

/// The fiber-engine image of a scope module, or `None` when the
/// engine refuses it or it disagrees with the module's program; the
/// module's kernels then stay on the interpreter.
pub fn module_image(module: &OpTemplateModule, context: &str) -> Option<Arc<dyn KernelProgram>> {
    match module.program_on(fiber_engine()) {
        Ok(image) if agrees(module.program(), &image) => Some(image),
        Ok(_) => {
            warn_interpreter(
                context,
                "its compiled image lists inputs or outputs differently",
            );
            None
        }
        Err(e) => {
            warn_interpreter(context, &e.to_string());
            None
        }
    }
}

/// The fiber-engine image of a scope compiled from `source` under
/// `options`, the same inputs that produced `program`; `None`, with a
/// warning, when it cannot stand in for `program`.
pub fn source_image(
    program: &PolydatProgram,
    source: &str,
    options: &polydat::dsl::compile::CompileOptions,
    context: &str,
) -> Option<Arc<dyn KernelProgram>> {
    let options = polydat::dsl::compile::CompileOptions {
        engine: fiber_engine(),
        ..options.clone()
    };
    match polydat::dsl::compile::compile_polydat_kernel_with_options(source, &options, None) {
        Ok(kernel) => {
            let image = kernel.into_program();
            if agrees(program, &image) {
                Some(image)
            } else {
                warn_interpreter(
                    context,
                    "its compiled image lists inputs or outputs differently",
                );
                None
            }
        }
        Err(e) => {
            warn_interpreter(context, &e.to_string());
            None
        }
    }
}

fn warn_interpreter(context: &str, reason: &str) {
    crate::diag!(
        crate::observer::LogLevel::Warn,
        "{context}: kernels stay on the interpreter ({} unavailable: {reason})",
        fiber_engine()
    );
}
