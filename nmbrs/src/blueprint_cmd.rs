// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `nmbrs blueprint` — authoring support for the SRD-108/109
//! blueprint/implementation composition.
//!
//! - `blueprint list` — every blueprint the binary can see:
//!   bundled workloads carrying unbound `abstract:` slots.
//! - `blueprint template <blueprint> [<out.yaml>]` — generate a
//!   syntactically correct implementation skeleton from a
//!   blueprint's typed interfaces: one stub op per abstract slot,
//!   with the slot's `needs` / `yields` / `results` contracts as
//!   guidance comments and the delivery surfaces (`capture:` /
//!   `result:`) pre-wired with TODO markers. The generated file
//!   BINDS against its blueprint as-is (total slot coverage, all
//!   promised wires declared) — filling the TODOs is protocol
//!   work, not plumbing work.

use nmbrs_workload::model::{ParsedOp, Workload};

pub fn blueprint_command(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("list") => {
            if let Err(e) = list_blueprints() {
                eprintln!("error: {e}");
                std::process::exit(2);
            }
        }
        Some("template") => {
            if let Err(e) = template(&args[1..]) {
                eprintln!("error: {e}");
                std::process::exit(2);
            }
        }
        _ => {
            eprintln!("nmbrs blueprint <subcommand>");
            eprintln!(
                "  list                           List blueprints (workloads with abstract op slots)"
            );
            eprintln!(
                "  template <blueprint> [<file>]  Generate an implementation skeleton (stdout, or <file>)"
            );
            eprintln!();
            eprintln!("A blueprint is a protocol-agnostic workload whose ops are typed");
            eprintln!("`abstract:` slots (SRD-108); an implementation binds literal op");
            eprintln!("bodies into those slots via `implements:` (see SRD-109 for the");
            eprintln!("http-driver form).");
        }
    }
}

/// CLI-spec registration: the `blueprint` umbrella with its
/// `list` / `template` subcommands.
pub fn spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle_default(p: ParsedCommand) -> Result<(), String> {
        blueprint_command(&p.positionals);
        Ok(())
    }
    fn handle_list(_p: ParsedCommand) -> Result<(), String> {
        blueprint_command(&["list".to_string()]);
        Ok(())
    }
    fn handle_template(p: ParsedCommand) -> Result<(), String> {
        let mut argv = vec!["template".to_string()];
        argv.extend(p.raw.iter().cloned());
        blueprint_command(&argv);
        Ok(())
    }
    Command {
        name: "blueprint",
        help: "Blueprint authoring support: list blueprints, scaffold an implementation (SRD-108/109).",
        category: Category::Documentation,
        level: Level::FullSurface,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        subcommands: vec![
            Command {
                name: "list",
                help: "List blueprints — bundled workloads carrying abstract op slots.",
                category: Category::Documentation,
                level: Level::FullSurface,
                flags: Vec::new(),
                kv_params: &[],
                dynamic_options: None,
                positionals: Vec::new(),
                subcommands: Vec::new(),
                handler: Some(Handler::Sync(handle_list)),
                raw_args: false,
                completion_override: None,
            },
            Command {
                name: "template",
                help: "Generate an implementation skeleton from a blueprint's typed interfaces.",
                category: Category::Documentation,
                level: Level::FullSurface,
                flags: Vec::new(),
                kv_params: &[],
                dynamic_options: None,
                positionals: vec![
                    crate::cli_spec::Positional {
                        name: "blueprint",
                        help: "Blueprint to implement (catalog name or local file).",
                        kind: crate::cli_spec::PositionalKind::One,
                        value: crate::cli_spec::ValueProvider::Custom(
                            crate::completion::catalog_name_provider,
                        ),
                    },
                    crate::cli_spec::Positional {
                        name: "out",
                        help: "Output file (default: stdout; existing files are never overwritten).",
                        kind: crate::cli_spec::PositionalKind::ZeroOrOne,
                        value: crate::cli_spec::ValueProvider::Path,
                    },
                ],
                subcommands: Vec::new(),
                handler: Some(Handler::Sync(handle_template)),
                raw_args: true,
                completion_override: None,
            },
        ],
        handler: Some(Handler::Sync(handle_default)),
        raw_args: false,
        completion_override: None,
    }
}

/// Load a blueprint by catalog name or local path, parsed with no
/// invocation params (the same loading `describe workloads` uses).
fn load_blueprint(name: &str) -> Result<(String, Workload), String> {
    use nmbrs_workload::catalog;
    let (identity, (merged, warnings)) = if let Some(b) = catalog::lookup(name) {
        (
            b.name.to_string(),
            nmbrs_workload::extends::load_and_merge_bundled(b)?,
        )
    } else if std::path::Path::new(name).exists() {
        let merged = nmbrs_workload::extends::load_and_merge(std::path::Path::new(name))?;
        (name.to_string(), merged)
    } else {
        return Err(format!(
            "`{name}` is neither a bundled workload nor a local file — \
             `nmbrs blueprint list` names the bundled blueprints"
        ));
    };
    for w in &warnings {
        eprintln!("warning: resolve: {w}");
    }
    let params = std::collections::HashMap::new();
    let workload = nmbrs_workload::parse::parse_workload(&merged, &params)
        .map_err(|e| format!("parse `{identity}`: {e}"))?;
    Ok((identity, workload))
}

/// List bundled blueprints. A cheap text scan narrows the catalog
/// to `abstract:` candidates; each candidate is then actually
/// parsed and kept only when it carries unbound abstract slots —
/// no claim without reading.
fn list_blueprints() -> Result<(), String> {
    let mut rows: Vec<(String, usize, String)> = Vec::new();
    for entry in nmbrs_workload::catalog::iter() {
        if !entry.source.contains("abstract:") {
            continue;
        }
        let Ok((identity, workload)) = load_blueprint(entry.name) else {
            continue;
        };
        let slots = nmbrs_workload::implements::unbound_abstract_slots(&workload);
        if slots.is_empty() {
            continue;
        }
        let desc = workload
            .description
            .as_deref()
            .and_then(|d| d.lines().next())
            .unwrap_or("")
            .to_string();
        rows.push((identity, slots.len(), desc));
    }
    if rows.is_empty() {
        println!("no blueprints in the bundled catalog");
        return Ok(());
    }
    let name_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(4).max(4);
    println!("{:name_w$}  {:>5}  {}", "name", "slots", "description");
    for (name, slots, desc) in &rows {
        println!("{name:name_w$}  {slots:>5}  {desc}");
    }
    println!();
    println!("scaffold one: nmbrs blueprint template <name> [<out.yaml>]");
    Ok(())
}

/// `blueprint template <blueprint> [<out.yaml>]`.
fn template(args: &[String]) -> Result<(), String> {
    let Some(name) = args.first() else {
        return Err("usage: nmbrs blueprint template <blueprint> [<out.yaml>]".into());
    };
    let (identity, workload) = load_blueprint(name)?;
    let slots = nmbrs_workload::implements::unbound_abstract_slots(&workload);
    if slots.is_empty() {
        return Err(format!(
            "`{identity}` declares no abstract op slots — it is not a \
             blueprint (nothing to implement)"
        ));
    }
    let rendered = generate_template(&identity, &workload);
    match args.get(1) {
        None => {
            print!("{rendered}");
        }
        Some(out) => {
            let path = std::path::Path::new(out);
            if path.exists() {
                return Err(format!(
                    "`{out}` already exists — refusing to overwrite; remove it \
                     or choose another name"
                ));
            }
            std::fs::write(path, &rendered).map_err(|e| format!("write `{out}`: {e}"))?;
            println!(
                "wrote {out} — implementation skeleton for `{identity}` \
                      ({} slot(s); fill the TODOs)",
                slots.len()
            );
        }
    }
    Ok(())
}

/// Render the implementation skeleton for a blueprint.
fn generate_template(identity: &str, workload: &Workload) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "#!/usr/bin/env nmbrs");
    let _ = writeln!(
        out,
        "# Implementation skeleton for the `{identity}` blueprint,"
    );
    let _ = writeln!(out, "# generated by `nmbrs blueprint template {identity}`.");
    let _ = writeln!(out, "#");
    let _ = writeln!(
        out,
        "# Fill each TODO with your protocol's LITERAL request form."
    );
    let _ = writeln!(
        out,
        "# An implementation phase carries ONLY `ops:` — scaffolding"
    );
    let _ = writeln!(
        out,
        "# (cycles, concurrency, governance, metrics, evaluations)"
    );
    let _ = writeln!(
        out,
        "# stays in the blueprint, and redeclaring it here is a load"
    );
    let _ = writeln!(
        out,
        "# error. Interface types are proven at synthesis; this file"
    );
    let _ = writeln!(
        out,
        "# already BINDS as-is (every slot covered, every promised"
    );
    let _ = writeln!(out, "# wire declared).");
    let _ = writeln!(out, "#");
    let _ = writeln!(
        out,
        "#   nmbrs run workload=<this-file> [scenario=…] [param=value …]"
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "description: |");
    let _ = writeln!(out, "  TODO: describe this implementation of {identity}.");
    let _ = writeln!(out);
    let _ = writeln!(out, "implements: {identity}");
    let _ = writeln!(out);

    // The blueprint's own params, for reference — the
    // implementation adds only protocol matter (a name both sides
    // declare is a load error).
    if !workload.declared_params.is_empty() {
        let _ = writeln!(
            out,
            "# The blueprint already declares these params (reference"
        );
        let _ = writeln!(out, "# them as {{name}}; do NOT redeclare):");
        let mut names = workload.declared_params.clone();
        names.sort();
        for p in &names {
            let v = workload.params.get(p).cloned().unwrap_or_default();
            let _ = writeln!(out, "#   {p} = {v}");
        }
    }
    let _ = writeln!(out, "params:");
    let _ = writeln!(
        out,
        "  # TODO: protocol params (endpoints, credentials, knobs)."
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "phases:");

    for phase_name in &workload.phase_order {
        let Some(phase) = workload.phases.get(phase_name) else {
            continue;
        };
        let abstract_ops: Vec<&ParsedOp> = phase
            .ops
            .iter()
            .filter(|op| op.abstract_interface.is_some())
            .collect();
        if abstract_ops.is_empty() {
            continue;
        }
        let _ = writeln!(out);
        let _ = writeln!(out, "  {phase_name}:");
        let _ = writeln!(out, "    ops:");
        for op in abstract_ops {
            let _ = writeln!(out);
            let _ = writeln!(out, "      {}:", op.name);
            if let Some(desc) = &op.description {
                for line in desc.lines() {
                    let _ = writeln!(out, "        # {line}");
                }
            }
            let iface = op
                .abstract_interface
                .as_ref()
                .expect("filtered to abstract ops");
            if iface.needs.is_empty() && iface.yields.is_empty() && iface.results.is_empty() {
                let _ = writeln!(
                    out,
                    "        # Free-form slot — no typed interface; any op body."
                );
            }
            if !iface.needs.is_empty() {
                let _ = writeln!(
                    out,
                    "        # needs — wires the blueprint GUARANTEES; reference"
                );
                let _ = writeln!(out, "        # them as {{name}} in the op body:");
                for (n, t) in &iface.needs {
                    let _ = writeln!(out, "        #   {n}: {t}");
                }
            }
            let _ = writeln!(
                out,
                "        stmt: \"TODO: {phase_name}.{} request\"",
                op.name
            );
            if !iface.yields.is_empty() {
                let _ = writeln!(
                    out,
                    "        # yields — wires you MUST deliver; point each capture"
                );
                let _ = writeln!(out, "        # at the response location that carries it:");
                let _ = writeln!(out, "        capture:");
                for (n, t) in &iface.yields {
                    let _ = writeln!(out, "          {n}: /TODO_pointer_to_{t}");
                }
            }
            if !iface.results.is_empty() {
                let _ = writeln!(
                    out,
                    "        # results — projected wires you MUST deliver; give each"
                );
                let _ = writeln!(
                    out,
                    "        # a path expression over the response body ([*] projects"
                );
                let _ = writeln!(out, "        # a column across every row — SRD-70):");
                let _ = writeln!(out, "        result:");
                for (n, t) in &iface.results {
                    let _ = writeln!(out, "          {n}: \"TODO_rows[*].TODO_{t}_column\"");
                }
            }
        }
    }
    out
}
