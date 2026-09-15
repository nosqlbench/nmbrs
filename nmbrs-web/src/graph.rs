// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Graph editor backend: palette API and graph JSON → Polydat source compilation.
//!
//! The palette API serves the function registry as JSON for the
//! Litegraph.js client to register node types. The compile endpoint
//! converts a Litegraph graph JSON into Polydat source, compiles it, and
//! returns the source text, SVG visualization, and sample output.

use std::collections::HashMap;

use polydat::dsl::registry;
use polydat::viz;
use serde::{Deserialize, Serialize};

// ─── Palette API ────────────────────────────────────────────

/// A category of functions for the node palette.
#[derive(Serialize)]
pub struct PaletteCategory {
    pub category: String,
    pub functions: Vec<PaletteFunction>,
}

/// A single function in the palette.
#[derive(Serialize)]
pub struct PaletteFunction {
    pub name: String,
    pub description: String,
    pub help: String,
    pub wire_inputs: usize,
    pub outputs: usize,
    pub const_params: Vec<PaletteParam>,
    pub variadic: bool,
}

/// A constant parameter on a function.
#[derive(Serialize)]
pub struct PaletteParam {
    pub name: String,
    pub required: bool,
}

/// Build the palette from the Polydat function registry.
pub fn build_palette() -> Vec<PaletteCategory> {
    let grouped = registry::by_category();
    grouped
        .into_iter()
        .map(|(cat, funcs)| PaletteCategory {
            category: cat.display_name().to_string(),
            functions: funcs
                .iter()
                .map(|sig| PaletteFunction {
                    name: sig.name.to_string(),
                    description: sig.description.to_string(),
                    help: sig.help.to_string(),
                    wire_inputs: sig.wire_input_count(),
                    outputs: if sig.outputs == 0 { 1 } else { sig.outputs },
                    const_params: sig
                        .const_param_info()
                        .iter()
                        .map(|(name, req)| PaletteParam {
                            name: name.to_string(),
                            required: *req,
                        })
                        .collect(),
                    variadic: sig.is_variadic(),
                })
                .collect(),
        })
        .collect()
}

// ─── Graph Compilation ──────────────────────────────────────

/// Litegraph serialized graph (subset of fields we need).
#[derive(Deserialize)]
pub struct LiteGraph {
    #[serde(default)]
    pub nodes: Vec<LiteNode>,
    #[serde(default)]
    pub links: Vec<serde_json::Value>,
}

/// A node in the Litegraph graph.
#[derive(Deserialize)]
pub struct LiteNode {
    pub id: i64,
    #[serde(rename = "type")]
    pub node_type: String,
    #[serde(default)]
    pub properties: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub inputs: Vec<LiteSlot>,
    #[serde(default)]
    pub outputs: Vec<LiteSlot>,
    #[serde(default)]
    pub widgets_values: Vec<serde_json::Value>,
}

/// A slot (input or output) on a node.
#[derive(Deserialize)]
pub struct LiteSlot {
    pub name: String,
    #[serde(rename = "type")]
    pub slot_type: Option<String>,
    #[serde(default)]
    pub link: Option<serde_json::Value>,
}

/// Result of compiling a graph.
#[derive(Serialize)]
pub struct CompileResult {
    pub polydat_source: String,
    pub svg: String,
    pub samples: Vec<String>,
    pub error: Option<String>,
}

/// Convert a Litegraph JSON graph into Polydat source text, compile it,
/// render SVG, and produce sample output for a few cycles.
pub fn compile_graph(graph_json: &str) -> CompileResult {
    let graph: LiteGraph = match serde_json::from_str(graph_json) {
        Ok(g) => g,
        Err(e) => {
            return CompileResult {
                polydat_source: String::new(),
                svg: String::new(),
                samples: vec![],
                error: Some(format!("invalid graph JSON: {e}")),
            };
        }
    };

    let translation = match graph_to_polydat(&graph) {
        Ok(t) => t,
        Err(e) => {
            return CompileResult {
                polydat_source: String::new(),
                svg: String::new(),
                samples: vec![],
                error: Some(e),
            };
        }
    };

    let polydat_source = translation.source;

    if polydat_source.trim().is_empty() {
        return CompileResult {
            polydat_source,
            svg: String::new(),
            samples: vec![],
            error: None,
        };
    }

    let svg = viz::polydat_to_svg(&polydat_source).unwrap_or_default();

    // Compile and sample a few cycles.
    let samples = match polydat::dsl::compile_polydat(&polydat_source) {
        Ok(mut kernel) => {
            let output_names: Vec<String> = kernel
                .output_names()
                .iter()
                .map(|s| s.to_string())
                .collect();
            (0..5u64)
                .map(|cycle| {
                    kernel.set_inputs(&[cycle]);
                    let vals: Vec<String> = output_names
                        .iter()
                        .map(|name| {
                            let v = kernel.pull(name);
                            format!("{name}={}", v.to_display_string())
                        })
                        .collect();
                    format!("cycle {cycle}: {}", vals.join(", "))
                })
                .collect()
        }
        Err(e) => {
            return CompileResult {
                polydat_source,
                svg,
                samples: vec![],
                error: Some(format!("compile error: {e}")),
            };
        }
    };

    CompileResult {
        polydat_source,
        svg,
        samples,
        error: None,
    }
}

/// Result of translating a Litegraph graph into Polydat source text.
pub struct PolydatTransition {
    /// The generated Polydat source code.
    pub source: String,
    /// Maps LiteGraph node ID to a list of (slot_index, var_name).
    pub var_map: HashMap<i64, Vec<(usize, String)>>,
}

/// Convert a Litegraph graph structure into Polydat source text.
fn graph_to_polydat(graph: &LiteGraph) -> Result<PolydatTransition, String> {
    if graph.nodes.is_empty() {
        return Ok(PolydatTransition {
            source: String::new(),
            var_map: HashMap::new(),
        });
    }

    // Build link map: link_id → (source_node_id, source_slot, dest_node_id, dest_slot)
    // Litegraph link format: [link_id, origin_id, origin_slot, target_id, target_slot, type]
    let mut link_map: HashMap<i64, (i64, usize)> = HashMap::new(); // link_id → (source_node, source_slot)
    for link_val in &graph.links {
        if let Some(arr) = link_val.as_array()
            && arr.len() >= 5
        {
            let link_id = arr[0].as_i64().unwrap_or(-1);
            let origin_id = arr[1].as_i64().unwrap_or(-1);
            let origin_slot = arr[2].as_u64().unwrap_or(0) as usize;
            link_map.insert(link_id, (origin_id, origin_slot));
        }
    }

    // Build node output name map: (node_id, slot_index) → variable name
    let mut output_names: HashMap<(i64, usize), String> = HashMap::new();

    // Find coordinate nodes and collect their output names.
    let mut coord_names: Vec<String> = Vec::new();
    for node in &graph.nodes {
        if node.node_type == "polydat/coordinates" {
            for (i, out) in node.outputs.iter().enumerate() {
                let name = out.name.clone();
                coord_names.push(name.clone());
                output_names.insert((node.id, i), name);
            }
        }
    }

    if coord_names.is_empty() {
        coord_names.push("cycle".into());
    }

    // Assign output variable names for function nodes.
    for node in &graph.nodes {
        if node.node_type == "polydat/coordinates"
            || node.node_type == "polydat/output"
            || node.node_type == "polydat/plotter"
        {
            continue;
        }
        // Node type is "Category/funcname" or "polydat/special" — extract last segment
        let func_name = node.node_type.rsplit('/').next().unwrap_or(&node.node_type);
        if node.outputs.len() == 1 {
            // Single output: use the node's custom name or func_id.
            let var_name = node
                .properties
                .get("output_name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("{}_{}", func_name, node.id));
            output_names.insert((node.id, 0), var_name);
        } else {
            for (i, out) in node.outputs.iter().enumerate() {
                let var_name = format!("{}_{}", out.name, node.id);
                output_names.insert((node.id, i), var_name);
            }
        }
    }

    // Helper: resolve what variable name feeds a given input slot.
    let resolve_input = |node: &LiteNode, slot_idx: usize| -> Option<String> {
        let input = node.inputs.get(slot_idx)?;
        // input.link can be a single link_id or null
        let link_id = match &input.link {
            Some(serde_json::Value::Number(n)) => n.as_i64()?,
            Some(serde_json::Value::Array(arr)) if !arr.is_empty() => arr[0].as_i64()?,
            _ => return None,
        };
        let (src_node, src_slot) = link_map.get(&link_id)?;
        output_names.get(&(*src_node, *src_slot)).cloned()
    };

    // Generate Polydat source. One `input` line per declared coordinate
    // (single-form) or a tuple `input (a: u64, b: u64, ...)` when
    // there's more than one.
    let mut lines = Vec::new();
    lines.push(match coord_names.as_slice() {
        [] => String::new(),
        [single] => format!("input {single}: u64"),
        many => {
            let typed: Vec<String> = many.iter().map(|n| format!("{n}: u64")).collect();
            format!("input ({})", typed.join(", "))
        }
    });

    // Topological order: process nodes in ID order (Litegraph assigns
    // incrementing IDs, which naturally respects creation order).
    let mut sorted_nodes: Vec<&LiteNode> = graph
        .nodes
        .iter()
        .filter(|n| {
            n.node_type != "polydat/coordinates"
                && n.node_type != "polydat/output"
                && n.node_type != "polydat/plotter"
        })
        .collect();
    sorted_nodes.sort_by_key(|n| n.id);

    for node in &sorted_nodes {
        // Node type is "Category/funcname" or "polydat/special" — extract last segment
        let func_name = node.node_type.rsplit('/').next().unwrap_or(&node.node_type);

        // Collect wire inputs.
        let mut args: Vec<String> = Vec::new();
        for i in 0..node.inputs.len() {
            if let Some(src) = resolve_input(node, i) {
                args.push(src);
            } else {
                // Unconnected input — use the first declared coordinate
                // as a placeholder so the emitted source still parses.
                // `coord_names` is guaranteed non-empty (the early default
                // upstream covers the no-coord-node case).
                args.push(coord_names[0].clone());
            }
        }

        // Collect const params from widget values.
        for widget_val in node.widgets_values.iter() {
            let val_str = match widget_val {
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::String(s) => {
                    // If it parses as a number, emit raw. Otherwise quote.
                    if s.parse::<f64>().is_ok() {
                        s.clone()
                    } else {
                        format!("\"{}\"", s.replace('"', "\\\""))
                    }
                }
                _ => continue,
            };
            args.push(val_str);
        }

        // Build the output targets.
        let targets: Vec<String> = (0..node.outputs.len())
            .filter_map(|i| output_names.get(&(node.id, i)).cloned())
            .collect();

        let target_str = if targets.len() == 1 {
            targets[0].clone()
        } else if targets.len() > 1 {
            format!("({})", targets.join(", "))
        } else {
            format!("{}_{}", func_name, node.id)
        };

        lines.push(format!(
            "{} := {}({})",
            target_str,
            func_name,
            args.join(", ")
        ));
    }

    let mut var_map: HashMap<i64, Vec<(usize, String)>> = HashMap::new();
    for ((node_id, slot_idx), var_name) in &output_names {
        var_map
            .entry(*node_id)
            .or_default()
            .push((*slot_idx, var_name.clone()));
    }
    Ok(PolydatTransition {
        source: lines.join("\n"),
        var_map,
    })
}

// ─── Eval API ──────────────────────────────────────────────

/// Request body for the eval endpoint.
#[derive(Deserialize)]
pub struct EvalRequest {
    pub graph: String,
    pub cycle: u64,
}

/// Full evaluation result including per-node port values.
#[derive(Serialize)]
pub struct EvalResult {
    pub polydat_source: String,
    pub svg: String,
    pub error: Option<String>,
    pub sample: String,
    pub node_values: HashMap<String, NodePortValues>,
}

/// Port values for a single node.
#[derive(Serialize)]
pub struct NodePortValues {
    pub outs: Vec<PortValue>,
}

/// A single port's evaluated value.
#[derive(Serialize)]
pub struct PortValue {
    pub name: String,
    pub value: String,
}

/// Request for batch evaluation over a cycle range (for plotting).
#[derive(Deserialize)]
pub struct PlotRequest {
    pub graph: String,
    pub cycle_start: u64,
    pub cycle_end: u64,
    pub cycle_step: u64,
}

/// Batch evaluation result: arrays of values per output, for plotting.
#[derive(Serialize)]
pub struct PlotResult {
    pub error: Option<String>,
    /// The cycle values (x-axis).
    pub cycles: Vec<u64>,
    /// Per-output arrays: output_name → array of values (as f64 for plotting).
    pub series: HashMap<String, Vec<f64>>,
    /// Output names in declaration order.
    pub output_names: Vec<String>,
}

/// Evaluate a graph over a range of cycles for plotting.
pub fn plot_graph(req: PlotRequest) -> PlotResult {
    let graph: LiteGraph = match serde_json::from_str(&req.graph) {
        Ok(g) => g,
        Err(e) => {
            return PlotResult {
                error: Some(format!("invalid graph JSON: {e}")),
                cycles: vec![],
                series: HashMap::new(),
                output_names: vec![],
            };
        }
    };

    let translation = match graph_to_polydat(&graph) {
        Ok(t) => t,
        Err(e) => {
            return PlotResult {
                error: Some(e),
                cycles: vec![],
                series: HashMap::new(),
                output_names: vec![],
            };
        }
    };

    if translation.source.trim().is_empty() {
        return PlotResult {
            error: None,
            cycles: vec![],
            series: HashMap::new(),
            output_names: vec![],
        };
    }

    match polydat::dsl::compile_polydat(&translation.source) {
        Ok(mut kernel) => {
            let output_names: Vec<String> = kernel
                .output_names()
                .iter()
                .map(|s| s.to_string())
                .collect();

            let step = req.cycle_step.max(1);
            // The range is caller-controlled: bound the sample count
            // before allocating or looping, and refuse an inverted
            // range, so one request can never drive the process out of
            // memory or into an unbounded evaluation loop.
            const MAX_SAMPLES: u64 = 100_000;
            let samples = match req.cycle_end.checked_sub(req.cycle_start) {
                Some(span) => span / step + 1,
                None => {
                    return PlotResult {
                        error: Some(format!(
                            "cycle_end ({}) is below cycle_start ({})",
                            req.cycle_end, req.cycle_start
                        )),
                        cycles: vec![],
                        series: HashMap::new(),
                        output_names,
                    };
                }
            };
            if samples > MAX_SAMPLES {
                return PlotResult {
                    error: Some(format!(
                        "range spans {samples} samples at step {step}; the plot limit is {MAX_SAMPLES} — narrow the range or raise cycle_step"
                    )),
                    cycles: vec![],
                    series: HashMap::new(),
                    output_names,
                };
            }
            let mut cycles = Vec::with_capacity(samples as usize);
            let mut series: HashMap<String, Vec<f64>> = HashMap::new();
            for name in &output_names {
                series.insert(name.clone(), Vec::new());
            }

            let mut c = req.cycle_start;
            while c <= req.cycle_end {
                cycles.push(c);
                kernel.set_inputs(&[c]);
                for name in &output_names {
                    let v = kernel.pull(name);
                    let f = match v {
                        polydat::ast::Value::U64(n) => *n as f64,
                        polydat::ast::Value::F64(n) => *n,
                        polydat::ast::Value::Bool(b) => {
                            if *b {
                                1.0
                            } else {
                                0.0
                            }
                        }
                        _ => f64::NAN,
                    };
                    series.get_mut(name).unwrap().push(f);
                }
                match c.checked_add(step) {
                    Some(next) => c = next,
                    None => break,
                }
            }

            PlotResult {
                error: None,
                cycles,
                series,
                output_names,
            }
        }
        Err(e) => PlotResult {
            error: Some(format!("compile error: {e}")),
            cycles: vec![],
            series: HashMap::new(),
            output_names: vec![],
        },
    }
}

/// Evaluate a graph at a specific cycle, returning per-node port values.
pub fn eval_graph(req: EvalRequest) -> EvalResult {
    let graph: LiteGraph = match serde_json::from_str(&req.graph) {
        Ok(g) => g,
        Err(e) => {
            return EvalResult {
                polydat_source: String::new(),
                svg: String::new(),
                error: Some(format!("invalid graph JSON: {e}")),
                sample: String::new(),
                node_values: HashMap::new(),
            };
        }
    };

    let translation = match graph_to_polydat(&graph) {
        Ok(t) => t,
        Err(e) => {
            return EvalResult {
                polydat_source: String::new(),
                svg: String::new(),
                error: Some(e),
                sample: String::new(),
                node_values: HashMap::new(),
            };
        }
    };

    if translation.source.trim().is_empty() {
        return EvalResult {
            polydat_source: translation.source,
            svg: String::new(),
            error: None,
            sample: String::new(),
            node_values: HashMap::new(),
        };
    }

    let svg = viz::polydat_to_svg(&translation.source).unwrap_or_default();

    match polydat::dsl::compile_polydat(&translation.source) {
        Ok(mut kernel) => {
            kernel.set_inputs(&[req.cycle]);

            // Pull all outputs and collect their display values.
            let output_names: Vec<String> = kernel
                .output_names()
                .iter()
                .map(|s| s.to_string())
                .collect();
            let mut all_values: HashMap<String, String> = HashMap::new();
            for name in &output_names {
                let v = kernel.pull(name);
                all_values.insert(name.clone(), v.to_display_string());
            }

            // Map variable values back to LiteGraph node IDs.
            let mut node_values: HashMap<String, NodePortValues> = HashMap::new();
            for (node_id, slots) in &translation.var_map {
                let mut outs = Vec::new();
                for (_, var_name) in slots {
                    if let Some(val) = all_values.get(var_name) {
                        outs.push(PortValue {
                            name: var_name.clone(),
                            value: val.clone(),
                        });
                    }
                }
                if !outs.is_empty() {
                    node_values.insert(node_id.to_string(), NodePortValues { outs });
                }
            }

            // Build sample string for sidebar.
            let sample_vals: Vec<String> = output_names
                .iter()
                .filter_map(|name| all_values.get(name).map(|v| format!("{name}={v}")))
                .collect();
            let sample = format!("cycle {}: {}", req.cycle, sample_vals.join(", "));

            EvalResult {
                polydat_source: translation.source,
                svg,
                error: None,
                sample,
                node_values,
            }
        }
        Err(e) => EvalResult {
            polydat_source: translation.source,
            svg,
            error: Some(format!("compile error: {e}")),
            sample: String::new(),
            node_values: HashMap::new(),
        },
    }
}
