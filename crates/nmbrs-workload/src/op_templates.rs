// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Op templates: reusable op definitions an op instantiates with
//! `uses:`.
//!
//! A document's top-level `op_templates:` maps a template name to an
//! op body — typically a protocol request shape (an http `method` /
//! `uri` / `body`) with a typed `abstract:` interface naming the wires
//! it `needs`. The templates run nowhere by themselves. An op in a
//! phase, a block, or the top-level `ops:` instantiates one:
//!
//! ```yaml
//! extends: petstore_ops           # a library of op_templates
//! phases:
//!   read:
//!     cycles: 1000
//!     ops:
//!       fetch:
//!         uses: getPetById        # the template
//!         bindings: |             # qualifies what it needs
//!           petId := mod(hash(cycle), 1000)
//! ```
//!
//! Resolution is a load-time document rewrite, before ops are parsed:
//! the op's body becomes the template's body with the op's own keys
//! folded in, so everything downstream sees an ordinary op. The rules
//! follow SRD-108's binder: a key both sides declare is a load error
//! (a template's request shape is fixed), except `params` — the
//! override surface, where the op re-defaults — and `tags`, which merge
//! with the op's winning. The instantiated op keeps the template's
//! `abstract:` interface as a *bound* interface: the parser checks that
//! every `needs` wire is supplied, and pre-map synthesis type-checks it
//! like any bound SRD-108 slot.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value as JVal};

/// The document key that declares op templates.
pub const OP_TEMPLATES_KEY: &str = "op_templates";

/// The op key that instantiates a template.
pub const USES_KEY: &str = "uses";

/// Which ops the rewrite instantiated from a template, by where they
/// live: the parser marks their interfaces bound and checks their needs.
#[derive(Debug, Default)]
pub(crate) struct Instantiated {
    /// Phase name → op names instantiated in that phase.
    pub phases: BTreeMap<String, BTreeSet<String>>,
    /// Op names instantiated in the top-level op pool (`ops:` and
    /// `blocks:`), from which tag-selected phases also draw.
    pub pool: BTreeSet<String>,
    /// Op name → the template it instantiates, for error messages.
    pub template_of: BTreeMap<String, String>,
}

/// Rewrite every `uses:` op in `doc` into the template it names, and
/// remove `op_templates:` from the document.
pub(crate) fn instantiate(doc: &mut Map<String, JVal>) -> Result<Instantiated, String> {
    let templates = match doc.remove(OP_TEMPLATES_KEY) {
        None => Map::new(),
        Some(JVal::Object(m)) => m,
        Some(other) => {
            return Err(format!(
                "`{OP_TEMPLATES_KEY}:` must be a mapping of template name -> op body, got {other}"
            ));
        }
    };
    for (name, body) in &templates {
        if !body.is_object() {
            return Err(format!(
                "op template '{name}' must be a mapping (an op body), got {body}"
            ));
        }
        if body.get(USES_KEY).is_some() {
            return Err(format!(
                "op template '{name}' declares `uses:` — a template is a complete \
                 op body and does not instantiate another"
            ));
        }
    }

    let mut out = Instantiated::default();
    if let Some(ops) = doc.get_mut("ops") {
        instantiate_ops(
            ops,
            &templates,
            "top-level ops",
            &mut out.pool,
            &mut out.template_of,
        )?;
    }
    if let Some(JVal::Object(blocks)) = doc.get_mut("blocks") {
        for (block_name, block) in blocks.iter_mut() {
            if let Some(ops) = block.get_mut("ops") {
                instantiate_ops(
                    ops,
                    &templates,
                    &format!("block '{block_name}'"),
                    &mut out.pool,
                    &mut out.template_of,
                )?;
            }
        }
    }
    if let Some(JVal::Object(phases)) = doc.get_mut("phases") {
        for (phase_name, phase) in phases.iter_mut() {
            if let Some(ops) = phase.get_mut("ops") {
                let mut used = BTreeSet::new();
                instantiate_ops(
                    ops,
                    &templates,
                    &format!("phase '{phase_name}'"),
                    &mut used,
                    &mut out.template_of,
                )?;
                if !used.is_empty() {
                    out.phases.insert(phase_name.clone(), used);
                }
            }
        }
    }
    Ok(out)
}

/// Instantiate the `uses:` ops of one op container, in both the map
/// form (`name: body`) and the list form (`- {name: …, uses: …}` or
/// `- name: {uses: …}`).
fn instantiate_ops(
    ops: &mut JVal,
    templates: &Map<String, JVal>,
    container: &str,
    used: &mut BTreeSet<String>,
    template_of: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    match ops {
        JVal::Object(map) => {
            for (name, body) in map.iter_mut() {
                if let JVal::Object(op) = body
                    && op.contains_key(USES_KEY)
                {
                    let template = instantiate_one(op, templates, name, container)?;
                    template_of.insert(name.clone(), template);
                    used.insert(name.clone());
                }
            }
        }
        JVal::Array(items) => {
            for item in items.iter_mut() {
                let JVal::Object(entry) = item else { continue };
                if entry.contains_key(USES_KEY) {
                    let name = entry
                        .get("name")
                        .and_then(JVal::as_str)
                        .ok_or_else(|| {
                            format!("{container}: a list-form op with `uses:` needs a `name:`")
                        })?
                        .to_string();
                    let template = instantiate_one(entry, templates, &name, container)?;
                    template_of.insert(name.clone(), template);
                    used.insert(name);
                } else if entry.len() == 1 {
                    let (name, body) = entry.iter_mut().next().expect("one entry");
                    if let JVal::Object(op) = body
                        && op.contains_key(USES_KEY)
                    {
                        let template = instantiate_one(op, templates, name, container)?;
                        template_of.insert(name.clone(), template);
                        used.insert(name.clone());
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Replace `op` with the template it names, its own keys folded in.
/// Returns the template's name.
fn instantiate_one(
    op: &mut Map<String, JVal>,
    templates: &Map<String, JVal>,
    op_name: &str,
    container: &str,
) -> Result<String, String> {
    let template_name = match op.remove(USES_KEY) {
        Some(JVal::String(s)) => s,
        Some(other) => {
            return Err(format!(
                "{container}: op '{op_name}': `uses:` must name an op template, got {other}"
            ));
        }
        None => unreachable!("called only for ops with `uses:`"),
    };
    let Some(JVal::Object(template)) = templates.get(&template_name) else {
        let known: Vec<&str> = templates.keys().map(String::as_str).collect();
        return Err(format!(
            "{container}: op '{op_name}' uses '{template_name}', but no op template \
             has that name (known: [{}]) — declare it under `{OP_TEMPLATES_KEY}:` or \
             `extends:` the library that does",
            known.join(", ")
        ));
    };
    let mut merged = template.clone();
    for (key, value) in std::mem::take(op) {
        match merged.get_mut(&key) {
            None => {
                merged.insert(key, value);
            }
            // The override surface: the op re-defaults a template param.
            Some(JVal::Object(base)) if key == "params" || key == "tags" => {
                let JVal::Object(over) = value else {
                    return Err(format!(
                        "{container}: op '{op_name}': `{key}:` must be a mapping"
                    ));
                };
                base.extend(over);
            }
            Some(_) => {
                return Err(format!(
                    "{container}: op '{op_name}' sets `{key}`, which op template \
                     '{template_name}' already defines — a template's request shape \
                     is fixed; qualify it through the wires it needs (bindings or \
                     params), or declare a new template"
                ));
            }
        }
    }
    *op = merged;
    Ok(template_name)
}

/// Mark every op instantiated from a template as a *bound* interface,
/// and check that each one's `needs` are supplied.
///
/// Runs on the parsed workload, after tag-selected phases have drawn
/// their ops from the pool (a selected clone of an instantiated pool op
/// is itself instantiated). A need is supplied by any wire the op can
/// resolve: a declared workload param, a workload, phase, or op binding,
/// the phase's `for_each` variables, or a name the scenario tree binds
/// (`set:` / `bindings:` / `for_each` / a do-loop counter). An op's own
/// `params:` are activity settings, not wires, so they supply nothing.
/// The types are proved later, at pre-map synthesis, like any bound
/// SRD-108 interface.
pub(crate) fn bind_and_check(
    inst: &Instantiated,
    phases: &mut std::collections::HashMap<String, crate::model::WorkloadPhase>,
    pool: &mut [crate::model::ParsedOp],
    declared_params: &[String],
    doc_bindings: &crate::model::BindingsDef,
    scenarios: &std::collections::HashMap<String, Vec<crate::model::ScenarioNode>>,
) -> Result<(), String> {
    for op in pool.iter_mut() {
        if inst.pool.contains(&op.name) && op.abstract_interface.is_some() {
            op.interface_bound = true;
        }
    }
    if inst.pool.is_empty() && inst.phases.is_empty() {
        return Ok(());
    }

    let mut workload_wide: BTreeSet<String> = declared_params.iter().cloned().collect();
    workload_wide.extend(binding_names(doc_bindings));
    for nodes in scenarios.values() {
        scenario_names(nodes, &mut workload_wide);
    }

    for (phase_name, phase) in phases.iter_mut() {
        let mut phase_wide = workload_wide.clone();
        phase_wide.extend(binding_names(&phase.bindings));
        if let Some(spec) = phase.for_each.as_deref()
            && let Ok(comp) = polydat::iteration::comprehension::spec::parse_inline(spec)
        {
            phase_wide.extend(comp.coordinate_names());
        }
        let in_phase = inst.phases.get(phase_name);
        for op in phase.ops.iter_mut() {
            let from_template =
                in_phase.is_some_and(|s| s.contains(&op.name)) || inst.pool.contains(&op.name);
            if !from_template {
                continue;
            }
            let Some(iface) = op.abstract_interface.as_ref() else {
                continue;
            };
            let mut provided = phase_wide.clone();
            provided.extend(binding_names(&op.bindings));
            for (need, typ) in &iface.needs {
                if !provided.contains(need) {
                    let template = inst.template_of.get(&op.name).map_or("?", String::as_str);
                    return Err(format!(
                        "op '{phase_name}.{}' uses op template '{template}', which needs \
                         '{need}' ({typ}) — supply it in the op's `bindings:`, or from \
                         the phase, workload, or scenario (a binding or a declared param)",
                        op.name
                    ));
                }
            }
            op.interface_bound = true;
        }
    }
    Ok(())
}

/// The wire names a bindings block declares.
fn binding_names(bindings: &crate::model::BindingsDef) -> Vec<String> {
    match bindings {
        crate::model::BindingsDef::PolydatSource(s) => crate::inline::binding_wire_names(s),
        crate::model::BindingsDef::Map(m) => m.keys().cloned().collect(),
    }
}

/// Every name a scenario tree binds for the phases beneath it.
fn scenario_names(nodes: &[crate::model::ScenarioNode], out: &mut BTreeSet<String>) {
    use crate::model::ScenarioNode as N;
    for node in nodes {
        match node {
            N::Phase(_) => {}
            N::Comprehension {
                comprehension,
                children,
                ..
            } => {
                out.extend(comprehension.coordinate_names());
                scenario_names(children, out);
            }
            N::DoWhile {
                counter, children, ..
            }
            | N::DoUntil {
                counter, children, ..
            } => {
                out.extend(counter.iter().cloned());
                scenario_names(children, out);
            }
            N::Bindings { source, children } => {
                out.extend(crate::inline::binding_wire_names(source));
                scenario_names(children, out);
            }
            N::IncludedScenario { children, .. } => scenario_names(children, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(v: JVal) -> Map<String, JVal> {
        v.as_object().expect("object").clone()
    }

    #[test]
    fn a_phase_op_becomes_its_template_with_its_own_keys() {
        let mut d = doc(json!({
            "op_templates": {
                "getPet": {
                    "abstract": {"needs": {"petId": "u64"}},
                    "method": "GET",
                    "uri": "{base_url}/pets/{petId}"
                }
            },
            "phases": {"read": {"ops": {"fetch": {
                "uses": "getPet",
                "bindings": "petId := 7"
            }}}}
        }));
        let out = instantiate(&mut d).expect("instantiates");
        assert!(d.get(OP_TEMPLATES_KEY).is_none());
        let op = &d["phases"]["read"]["ops"]["fetch"];
        assert_eq!(op["method"], "GET");
        assert_eq!(op["bindings"], "petId := 7");
        assert!(op.get(USES_KEY).is_none());
        assert!(out.phases["read"].contains("fetch"));
        assert_eq!(out.template_of["fetch"], "getPet");
    }

    #[test]
    fn redefining_a_template_field_is_a_load_error() {
        let mut d = doc(json!({
            "op_templates": {"getPet": {"method": "GET", "uri": "/pets"}},
            "phases": {"read": {"ops": {"fetch": {"uses": "getPet", "method": "POST"}}}}
        }));
        let err = instantiate(&mut d).unwrap_err();
        assert!(err.contains("sets `method`"), "{err}");
    }

    #[test]
    fn params_and_tags_merge_with_the_op_winning() {
        let mut d = doc(json!({
            "op_templates": {"t": {"stmt": "x", "params": {"a": "1", "b": "2"}, "tags": {"k": "v"}}},
            "ops": {"o": {"uses": "t", "params": {"b": "3"}, "tags": {"z": "q"}}}
        }));
        let out = instantiate(&mut d).expect("instantiates");
        let op = &d["ops"]["o"];
        assert_eq!(op["params"], json!({"a": "1", "b": "3"}));
        assert_eq!(op["tags"], json!({"k": "v", "z": "q"}));
        assert!(out.pool.contains("o"));
    }

    #[test]
    fn an_unknown_template_names_the_known_ones() {
        let mut d = doc(json!({
            "op_templates": {"getPet": {"stmt": "x"}},
            "phases": {"p": {"ops": [{"name": "o", "uses": "nope"}]}}
        }));
        let err = instantiate(&mut d).unwrap_err();
        assert!(err.contains("'nope'") && err.contains("getPet"), "{err}");
    }
}
