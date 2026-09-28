// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Wiring / value-provenance analysis output for diagnostic modes.
//!
//! Called by the runner when `dryrun=wiring` is active. Renders
//! the variable connections between named wires at each kernel
//! scope: how each output was computed, what inputs it depends
//! on, and where those inputs came from. The polydat runtime
//! kernel is the source of this data, but the user-facing
//! concept is "wiring" between named values, not the kernel
//! representation.

use polydat::kernel::PolydatProgram;
use std::sync::Arc;

/// Print wiring analysis for a phase/iteration scope.
///
/// Called by the runner at the point where it would normally
/// dispatch cycles. The kernel has already been compiled through
/// the exact same pipeline as execution.
pub fn print_wiring_analysis(phase_name: &str, iter_note: &str, program: &Arc<PolydatProgram>) {
    let input_names = program.input_names();
    let coord_count = program.coord_count();

    println!(
        "  Phase '{phase_name}'{iter_note} ({} nodes, {} outputs):",
        program.node_count(),
        program.output_count()
    );

    for (i, name) in input_names.iter().enumerate() {
        let kind = if i < coord_count {
            "coordinate"
        } else {
            "extern"
        };
        println!("    input {name}: {kind}");
    }

    for i in 0..program.output_count() {
        let name = program.output_name(i);
        let (node_idx, port_idx) = program.resolve_output_by_index(i);
        let meta = program.node_meta(node_idx);
        let provenance = program.input_provenance_for(node_idx);
        let modifier = program.output_modifier(name);
        let is_const = program.node_wiring(node_idx).is_empty();

        let mut deps: Vec<String> = Vec::new();
        for (j, inp_name) in input_names.iter().enumerate() {
            if provenance.is_some_and(|p| p.contains(j)) {
                deps.push(inp_name.clone());
            }
        }

        // Build the [final shared volatile] tag suffix from the
        // modifier's flag set. Order is fixed so the output is
        // stable.
        let mod_str = {
            let mut tags: Vec<&str> = Vec::new();
            if modifier.is_const() {
                tags.push("final");
            }
            if modifier.is_shared() {
                tags.push("shared");
            }
            if modifier.is_volatile() {
                tags.push("volatile");
            }
            if tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", tags.join(" "))
            }
        };
        let mod_str = mod_str.as_str();
        let out_type = if port_idx < meta.outs.len() {
            format!("{:?}", meta.outs[port_idx].typ)
        } else {
            "?".into()
        };

        print!("    {name}{mod_str}: {out_type}");
        if is_const {
            println!("  (const-folded at compile time)");
        } else if deps.is_empty() {
            println!("  (no input deps)");
        } else {
            println!("  (per-cycle, depends on: {})", deps.join(", "));
        }

        let wiring = program.node_wiring(node_idx);
        if !wiring.is_empty() {
            let descs: Vec<String> = wiring
                .iter()
                .map(|w| match w {
                    polydat::kernel::WireSource::Input(idx) => {
                        if *idx < input_names.len() {
                            format!("input:{}", input_names[*idx])
                        } else {
                            format!("input:{idx}")
                        }
                    }
                    polydat::kernel::WireSource::NodeOutput(ni, pi) => {
                        let u = program.node_meta(*ni);
                        if *pi == 0 {
                            u.name.clone()
                        } else {
                            format!("{}[{pi}]", u.name)
                        }
                    }
                })
                .collect();
            println!("      node: {}({})", meta.name, descs.join(", "));
        }
    }
    println!();
}
