// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The `describe` subcommand: introspect wiring functions, stdlib, modules, and DAGs.

use polydat::dsl::registry;

pub fn describe_command(args: &[String]) {
    let first = args.first().map(|s| s.as_str()).unwrap_or("");

    // `nmbrs describe adapter=<name>` / `nmbrs describe adapter`
    // shorthand. The `key=value` form mirrors the rest of nmbrs's
    // CLI, so it composes with the user's muscle memory.
    if let Some((topic, value)) = first.split_once('=')
        && topic == "adapter"
    {
        describe_adapter(value);
        return;
    }

    let topic = first;
    let subtopic = args.get(1).map(|s| s.as_str()).unwrap_or("");

    // Parse `--verbose` / `-v` flag from the args (applies to
    // wiring functions; ignored elsewhere for now).
    let verbose = args.iter().any(|a| a == "--verbose" || a == "-v");

    match (topic, subtopic) {
        ("adapter", "") => describe_adapters_list(),
        ("adapter", name) => describe_adapter(name),
        ("optimizers", "") | ("optimizer", "") => describe_optimizers_list(),
        ("optimizers", name) | ("optimizer", name) => describe_optimizer(name),
        ("controls", "") | ("control", "") => describe_controls(),
        ("controls", name) | ("control", name) => describe_control(name),
        ("wiring", "functions") => describe_wiring_functions(verbose),
        ("wiring", "functions-md") => {
            let rest: Vec<String> = args
                .iter()
                .skip(2)
                .filter(|a| !a.starts_with('-'))
                .cloned()
                .collect();
            let path = rest
                .first()
                .map(|s| s.as_str())
                .unwrap_or("wiring_functions.md");
            describe_wiring_functions_md(path);
        }
        ("wiring", "stdlib") => describe_wiring_stdlib(),
        ("wiring", "types") => describe_wiring_types(),
        ("wiring", "types-md") => {
            let rest: Vec<String> = args
                .iter()
                .skip(2)
                .filter(|a| !a.starts_with('-'))
                .cloned()
                .collect();
            let path = rest
                .first()
                .map(|s| s.as_str())
                .unwrap_or("wiring_types.md");
            describe_wiring_types_md(path);
        }
        ("wiring", "dag") => {
            // Remaining args after "describe wiring dag" are the wiring source or file
            let rest: Vec<String> = args.iter().skip(2).cloned().collect();
            describe_wiring_dag(&rest);
        }
        ("wiring", "modules") => {
            let rest: Vec<String> = args.iter().skip(2).cloned().collect();
            describe_wiring_modules(&rest);
        }
        ("wiring", _) => {
            eprintln!("nmbrs describe wiring <subtopic>");
            eprintln!(
                "  functions [--verbose]  List wiring functions (verbose: + signatures, types, associativity)"
            );
            eprintln!("  functions-md           Dump all wiring functions to a markdown file");
            eprintln!("  types                  List wiring port types with descriptions");
            eprintln!("  types-md               Dump wiring types to a markdown file");
            eprintln!("  stdlib                 List embedded standard library modules");
            eprintln!("  dag                    Render a wiring source as DOT, Mermaid, or SVG");
            eprintln!("  modules                List modules from a directory");
        }
        // SRD-32a Push 4 — discoverability commands.
        // `describe wrappers` dumps the wrapper registry; the
        // resolver isn't consulted here because the table is a
        // pure registry view.
        ("wrappers", _) => {
            print!("{}", render_wrappers_table());
        }
        // `describe op <workload> <op>` loads a workload, finds
        // the named op-template, and renders its resolved
        // wrapper stack with provenance.
        ("op", workload_path) if !workload_path.is_empty() => {
            let op_name = args.get(2).map(|s| s.as_str()).unwrap_or("");
            if op_name.is_empty() {
                eprintln!("nmbrs describe op <workload> <op>");
                eprintln!("  loads <workload>, finds the op template named <op>,");
                eprintln!("  and prints its resolved wrapper stack.");
                return;
            }
            match render_op_description(workload_path, op_name) {
                Ok(text) => print!("{text}"),
                Err(e) => eprintln!("error: {e}"),
            }
        }
        ("op", _) => {
            eprintln!("nmbrs describe op <workload> <op>");
            eprintln!("  loads <workload>, finds the op template named <op>,");
            eprintln!("  and prints its resolved wrapper stack.");
        }
        // SRD-85: the bundled-workload catalog.
        ("workloads", _) => {
            describe_workloads(&args[1..]);
        }
        // SRD-109: driver manifests (bundled + local).
        ("drivers", _) => {
            describe_drivers();
        }
        _ => {
            eprintln!("nmbrs describe <topic>");
            eprintln!(
                "  adapter[=<name>]   List adapters / show one adapter's params + drivers + controls"
            );
            eprintln!(
                "  controls [<name>]  List every dynamic control (SRD-23) the binary can declare"
            );
            eprintln!("  optimizers [<n>]   List registered optimizers / show one in detail");
            eprintln!(
                "  wiring             Wiring (graph-kernel) topics: functions, modules, dag, stdlib"
            );
            eprintln!("  workloads          List bundled workloads / show one in detail");
            eprintln!("  drivers            List driver manifests (SRD-109 web client drivers)");
            eprintln!("  wrappers           List the registered op-template wrappers");
            eprintln!("  op <wkl> <op>      Show the resolved wrapper stack for one op");
            eprintln!();
            eprintln!("For workload analysis, use: nmbrs run workload=file.yaml dryrun=op,wiring");
        }
    }
}

// =========================================================================
// SRD-109: `describe drivers` — driver-manifest discovery
// =========================================================================

/// List driver manifests: bundled (`drivers/<name>/driver` catalog
/// entries) plus local `./drivers/*/driver.yaml` under the cwd —
/// the same local-first pairing `driver=<name>` resolution uses.
fn describe_drivers() {
    struct Row {
        name: String,
        adapter: String,
        origin: &'static str,
        description: String,
    }
    let mut rows: Vec<Row> = Vec::new();

    for entry in nmbrs_workload::catalog::iter() {
        let Some(rest) = entry.name.strip_prefix("drivers/") else {
            continue;
        };
        let Some(driver_name) = rest.strip_suffix("/driver") else {
            continue;
        };
        match nmbrs_workload::drivers::parse_driver_manifest(entry.source, entry.name) {
            Ok(m) => rows.push(Row {
                name: driver_name.to_string(),
                adapter: m.adapter,
                origin: "bundled",
                description: m.description.unwrap_or_default(),
            }),
            Err(e) => eprintln!("warning: {e}"),
        }
    }
    if let Ok(dir) = std::fs::read_dir("drivers") {
        let mut local: Vec<_> = dir.flatten().collect();
        local.sort_by_key(|e| e.file_name());
        for entry in local {
            let manifest_path = entry.path().join("driver.yaml");
            if !manifest_path.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            match std::fs::read_to_string(&manifest_path)
                .map_err(|e| format!("read {}: {e}", manifest_path.display()))
                .and_then(|s| {
                    nmbrs_workload::drivers::parse_driver_manifest(
                        &s,
                        &manifest_path.display().to_string(),
                    )
                }) {
                Ok(m) => rows.push(Row {
                    name,
                    adapter: m.adapter,
                    origin: "local",
                    description: m.description.unwrap_or_default(),
                }),
                Err(e) => eprintln!("warning: {e}"),
            }
        }
    }

    if rows.is_empty() {
        println!("no driver manifests found (bundled drivers/ or local ./drivers/)");
        return;
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(4).max(4);
    let adapter_w = rows
        .iter()
        .map(|r| r.adapter.len())
        .max()
        .unwrap_or(7)
        .max(7);
    println!(
        "{:name_w$}  {:adapter_w$}  {:8}  {}",
        "name", "adapter", "origin", "description"
    );
    for r in &rows {
        let desc = r.description.lines().next().unwrap_or("");
        println!(
            "{:name_w$}  {:adapter_w$}  {:8}  {desc}",
            r.name, r.adapter, r.origin
        );
    }
    println!();
    println!("run: nmbrs run driver=<name> [workload=<blueprint>] [param=value …]");
}

// =========================================================================
// SRD-85: `describe workloads` — bundled-catalog discovery
// =========================================================================

/// Dispatch for `nmbrs describe workloads [...]`:
///
/// - *(no args)*   — list the curated tier.
/// - `--all`       — list curated + examples tiers.
/// - `examples`    — list the examples tier only.
/// - `--json`      — machine-readable listing (composes with the
///   selectors above).
/// - `<name>`      — one workload in detail (catalog name, or a
///   local path — the same renderer introspects
///   un-bundled files).
fn describe_workloads(args: &[String]) {
    use nmbrs_workload::catalog::{self, Tier};
    let all = args.iter().any(|a| a == "--all");
    let json = args.iter().any(|a| a == "--json");
    let positional = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(|s| s.as_str());

    match positional {
        Some("examples") => {
            list_workloads(catalog::iter_tier(Tier::Example).collect(), json);
        }
        Some(name) => {
            if let Err(e) = describe_one_workload(name) {
                eprintln!("error: {e}");
            }
        }
        None => {
            let entries: Vec<_> = if all {
                catalog::iter().collect()
            } else {
                catalog::iter_tier(Tier::Curated).collect()
            };
            list_workloads(entries, json);
            if !all && !json {
                let hidden = catalog::iter_tier(Tier::Example).count();
                if hidden > 0 {
                    println!();
                    println!(
                        "({hidden} example workloads bundled — `nmbrs describe \
                         workloads --all` lists them; all are runnable by name)"
                    );
                }
            }
        }
    }
}

/// One-line summary for a bundled workload: the `description:`
/// field's first line, falling back to the leading comment
/// block (the established header convention for examples).
fn workload_summary(source: &str) -> String {
    if let Ok(jval) = serde_yaml::from_str::<serde_json::Value>(source)
        && let Some(desc) = jval.get("description").and_then(|d| d.as_str())
        && let Some(first) = desc.lines().find(|l| !l.trim().is_empty())
    {
        return first.trim().to_string();
    }
    // Comment-block fallback: first non-empty, non-decorative
    // comment line (skipping shebangs, license headers, and the
    // bare-filename line examples conventionally start with).
    for line in source.lines() {
        let Some(stripped) = line.strip_prefix('#') else {
            break;
        };
        let t = stripped.trim();
        if t.is_empty()
            || t.starts_with('!')
            || t.starts_with("Copyright")
            || t.starts_with("SPDX")
            || t.ends_with(".yaml")
            || t.chars().all(|c| !c.is_alphanumeric())
        {
            continue;
        }
        // `<file>.yaml — actual summary` headers: keep the part
        // after the dash.
        for dash in [" — ", " - "] {
            if let Some((head, rest)) = t.split_once(dash)
                && head.ends_with(".yaml")
                && !rest.trim().is_empty()
            {
                return rest.trim().to_string();
            }
        }
        return t.to_string();
    }
    String::new()
}

fn list_workloads(entries: Vec<&'static nmbrs_workload::catalog::BundledWorkload>, json: bool) {
    if entries.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No bundled workloads in this binary for the selected tier.");
        }
        return;
    }
    if json {
        let items: Vec<serde_json::Value> = entries
            .iter()
            .map(|w| {
                // `described` = carries a structured `description:`
                // field (the curated-tier lint requirement), as
                // opposed to the comment-block fallback.
                let described = serde_yaml::from_str::<serde_json::Value>(w.source)
                    .ok()
                    .and_then(|j| {
                        j.get("description")
                            .and_then(|d| d.as_str().map(|s| !s.trim().is_empty()))
                    })
                    .unwrap_or(false);
                serde_json::json!({
                    "name": w.name,
                    "tier": w.tier.as_str(),
                    "summary": workload_summary(w.source),
                    "described": described,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items).unwrap());
        return;
    }
    let width = entries.iter().map(|w| w.name.len()).max().unwrap_or(0);
    for w in &entries {
        println!("  {:<width$}  {}", w.name, workload_summary(w.source));
    }
}

/// Detail view for one workload — catalog name or local path.
fn describe_one_workload(name: &str) -> Result<(), String> {
    use nmbrs_workload::catalog;
    let (identity, (merged, res_warnings), tier): (String, (String, Vec<String>), Option<&str>) =
        if let Some(b) = catalog::lookup(name) {
            let merged = nmbrs_workload::extends::load_and_merge_bundled(b)?;
            (b.name.to_string(), merged, Some(b.tier.as_str()))
        } else if std::path::Path::new(name).exists() {
            let merged = nmbrs_workload::extends::load_and_merge(std::path::Path::new(name))?;
            (name.to_string(), merged, None)
        } else {
            return Err(format!(
                "`{name}` is neither a bundled workload nor a local file — \
                 `nmbrs describe workloads` lists the catalog"
            ));
        };

    for w in &res_warnings {
        eprintln!("warning: resolve: {w}");
    }
    let params = std::collections::HashMap::new();
    let workload = nmbrs_workload::parse::parse_workload(&merged, &params)
        .map_err(|e| format!("parse `{identity}`: {e}"))?;

    println!("workload: {identity}");
    if let Some(t) = tier {
        println!("tier:     {t}");
    }
    if let Some(desc) = &workload.description {
        println!();
        for line in desc.lines() {
            println!("  {line}");
        }
    }
    if !workload.declared_params.is_empty() {
        println!();
        println!("params (defaults):");
        let mut names = workload.declared_params.clone();
        names.sort();
        for p in names {
            let v = workload.params.get(&p).cloned().unwrap_or_default();
            println!("  {p} = {v}");
        }
    }
    if !workload.scenarios.is_empty() {
        println!();
        println!("scenarios:");
        let mut names: Vec<_> = workload.scenarios.keys().collect();
        names.sort();
        for s in names {
            let steps = workload.scenarios[s].len();
            println!("  {s}  ({steps} step{})", if steps == 1 { "" } else { "s" });
        }
    }
    if !workload.phase_order.is_empty() {
        println!();
        println!("phases:");
        for p in &workload.phase_order {
            println!("  {p}");
        }
    }
    // Adapters referenced: workload param, phase-level, op-level.
    let mut adapters: Vec<String> = Vec::new();
    if let Some(a) = workload.params.get("adapter") {
        adapters.push(a.clone());
    }
    for phase in workload.phases.values() {
        if let Some(a) = &phase.adapter {
            adapters.push(a.clone());
        }
        for op in &phase.ops {
            if let Some(a) = op.op.get("adapter").and_then(|v| v.as_str()) {
                adapters.push(a.to_string());
            }
        }
    }
    adapters.sort();
    adapters.dedup();
    if !adapters.is_empty() {
        println!();
        println!("adapters: {}", adapters.join(", "));
    }
    println!();
    println!("run:      nmbrs run workload={identity} [scenario=<name>] [params...]");
    if tier.is_some() {
        println!("copy:     nmbrs copy {identity}");
    }
    Ok(())
}

/// The first non-heading, non-empty line of a markdown doc (its summary).
fn first_prose_line(md: &str) -> &str {
    md.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("")
}

/// `nmbrs describe optimizers` — list the optimizers registered in this binary
/// (the built-in `sweep` + the `nmbrs-optimizers` plugins discovered via
/// inventory against the core contract), each with its one-line summary
/// (SRD-86 §6).
fn describe_optimizers_list() {
    let infos = nmbrs_runtime::optimize::describe();
    println!("Registered optimizers:");
    for info in &infos {
        println!("  {:<22} {}", info.name, first_prose_line(info.doc_md));
    }
    println!();
    println!("For details: nmbrs describe optimizers <name>");
}

/// `nmbrs describe optimizers <name>` — print the optimizer's full markdown doc.
fn describe_optimizer(name: &str) {
    match nmbrs_runtime::optimize::describe()
        .into_iter()
        .find(|i| i.name == name)
    {
        Some(info) => print!("{}", info.doc_md),
        None => {
            eprintln!("No optimizer named '{name}' is registered in this binary.");
            eprintln!();
            describe_optimizers_list();
        }
    }
}

/// Render an f64 bound compactly for `describe` tables — integers without a
/// trailing `.0`, infinities as `∞`.
fn fmt_num(v: f64) -> String {
    if v.is_infinite() {
        return if v > 0.0 { "∞".into() } else { "-∞".into() };
    }
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Render one control's type + range + unit (e.g. `count(u32), [1, 100000] fibers`).
fn control_shape(d: &nmbrs_runtime::control_catalog::ControlDesc) -> String {
    use nmbrs_runtime::control_catalog::ControlValueType;
    let range = match d.value_type {
        // `rate`'s floor is exclusive and its ceiling unbounded — "> 0" reads
        // truer than `(0, ∞]`.
        ControlValueType::Rate => "> 0".to_string(),
        _ => format!("[{}, {}]", fmt_num(d.min), fmt_num(d.max)),
    };
    format!("{}, {} {}", d.value_type.label(), range, d.unit)
}

/// `nmbrs describe controls` — the static **capability** catalog of dynamic
/// controls (SRD-23). Lists every control the binary *can* declare — core plus
/// each registered adapter's — with the condition under which it appears, so a
/// user discovers knobs (including conditional ones like `rate`) without
/// running a workload. Complementary to `dryrun=controls`, which shows what a
/// *specific* run actually declares on the live component tree.
fn describe_controls() {
    use nmbrs_runtime::control_catalog::all_controls;

    let entries = all_controls();
    println!("Dynamic controls (SRD-23) — retarget live via the TUI `e` prompt, web POST,");
    println!("polydat `control_set`, or `optimize.servo`, without restarting a phase.");
    println!();
    if entries.is_empty() {
        println!("  (no dynamic controls registered in this binary)");
        return;
    }
    let name_w = entries
        .iter()
        .map(|e| e.desc.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let owner_w = entries
        .iter()
        .map(|e| e.owner.to_string().len())
        .max()
        .unwrap_or(5)
        .max(5);
    for e in &entries {
        let d = e.desc;
        println!(
            "  {:<name_w$}  {:<owner_w$}  {}",
            d.name,
            e.owner.to_string(),
            control_shape(d)
        );
        println!(
            "  {:<name_w$}  {:<owner_w$}  declared {}",
            "", "", d.declared_when
        );
        println!("  {:<name_w$}  {:<owner_w$}  {}", "", "", d.doc);
        println!();
    }
    println!("Servo any toward an objective with `optimize: {{ servo: <name> }}` (SRD-86 §4).");
    println!(
        "Run-specific view (what a given workload declares): nmbrs run workload=… dryrun=controls"
    );
}

/// `nmbrs describe controls <name>` — detail for one control, or the full list
/// with a hint if the name is unknown.
fn describe_control(name: &str) {
    use nmbrs_runtime::control_catalog::all_controls;

    match all_controls().into_iter().find(|e| e.desc.name == name) {
        Some(e) => {
            let d = e.desc;
            println!("Control: {}", d.name);
            println!("  Owner:        {}", e.owner);
            println!("  Shape:        {}", control_shape(d));
            println!("  Default:      {}", fmt_num(d.default));
            println!("  Declared:     {}", d.declared_when);
            println!("  Servo:    optimize: {{ servo: {} }}", d.name);
            println!();
            println!("  {}", d.doc);
        }
        None => {
            eprintln!("No dynamic control named '{name}' can be declared by this binary.");
            eprintln!();
            describe_controls();
        }
    }
}

fn describe_adapters_list() {
    use nmbrs_runtime::adapter::{default_drivers, registered_driver_names};

    let mut names = registered_driver_names();
    names.sort();
    names.dedup();
    if names.is_empty() {
        println!("No adapters registered in this binary.");
        return;
    }
    println!("Registered adapters:");
    for name in names {
        let drivers = default_drivers(name);
        if drivers.is_empty() {
            println!("  {name}");
        } else {
            // Multi-driver adapter — show the rank-derived default
            // and the alternative drivers compiled in.
            let default = drivers.first().copied().unwrap_or("");
            let alts: Vec<&str> = drivers.iter().skip(1).copied().collect();
            if alts.is_empty() {
                println!("  {name}    (driver: {default})");
            } else {
                println!(
                    "  {name}    (drivers: {default} [default], {})",
                    alts.join(", ")
                );
            }
        }
    }
    println!();
    println!("For details: nmbrs describe adapter=<name>");
}

fn describe_adapter(name: &str) {
    use nmbrs_runtime::adapter::{default_drivers, find_adapter_registration, find_driver};

    let Some(reg) = find_adapter_registration(name) else {
        eprintln!("No adapter named '{name}' is registered in this binary.");
        eprintln!();
        describe_adapters_list();
        return;
    };

    let aliases = (reg.names)();
    println!("Adapter: {name}");
    if aliases.len() > 1 {
        println!("  Aliases:        {}", aliases.join(", "));
    }
    // Default-config preference (no params → e.g. stdout's console default).
    println!(
        "  Display:        {:?}",
        (reg.display_preference)(&std::collections::HashMap::new())
    );

    let adapter_params = (reg.known_params)();
    if !adapter_params.is_empty() {
        println!("  Adapter params: {}", adapter_params.join(", "));
    }

    // SRD-23 — the dynamic controls this adapter can declare (capability view,
    // read off the registration without constructing the adapter).
    let controls = (reg.supported_controls)();
    if !controls.is_empty() {
        println!();
        println!("  Dynamic controls (SRD-23):");
        for d in controls {
            println!("    {:<16} {}", d.name, control_shape(d));
            println!("    {:<16} declared {}", "", d.declared_when);
        }
    }

    let drivers = default_drivers(name);
    if drivers.is_empty() {
        println!();
        return;
    }

    let default = drivers.first().copied().unwrap_or("");
    println!();
    println!("  Drivers (compiled into this binary, rank order — first is default):");
    for driver in &drivers {
        let marker = if *driver == default { " [default]" } else { "" };
        match find_driver(name, driver) {
            Some(impl_) => {
                let dparams = (impl_.known_params)();
                println!("    {driver}{marker}  rank={}", impl_.default_rank);
                if !dparams.is_empty() {
                    println!("      params: {}", dparams.join(", "));
                }
            }
            None => println!("    {driver}{marker}"),
        }
    }
    if drivers.len() > 1 {
        println!();
        // Selector convention: drivers are picked via
        // `<adapter>driver=…` (e.g. `cqldriver=scylla`). Surface
        // the exact knob + accepted values so the user doesn't
        // have to read the source.
        println!(
            "  Select a driver with: {name}driver=<{}>",
            drivers.join("|")
        );
    }
}

fn describe_wiring_functions(verbose: bool) {
    use nmbrs_runtime::bindings::probe_compile_level;

    let grouped = registry::by_category();
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());

    // ANSI color codes
    let (bold, dim, reset, green, cyan, magenta) = if is_tty {
        (
            "\x1b[1m", "\x1b[2m", "\x1b[0m", "\x1b[32m", "\x1b[36m", "\x1b[35m",
        )
    } else {
        ("", "", "", "", "", "")
    };

    println!();
    println!("{bold}Wiring Node Functions{reset}");
    println!("{bold}═════════════════════{reset}");
    println!();

    for (cat, funcs) in &grouped {
        let cat_name = cat.display_name();
        println!("  {bold}{cyan}── {cat_name} ──{reset}");
        println!();

        for sig in funcs {
            let level = probe_compile_level(sig.name);
            let (p1, p2, p3) = match level {
                registry::CompileLevel::Phase3 => (
                    format!("{green}\u{2713}{reset}"),
                    format!("{green}\u{2713}{reset}"),
                    format!("{green}\u{2713}{reset}"),
                ),
                registry::CompileLevel::Phase2 => (
                    format!("{green}\u{2713}{reset}"),
                    format!("{green}\u{2713}{reset}"),
                    format!("{dim}\u{2717}{reset}"),
                ),
                registry::CompileLevel::Phase1 => (
                    format!("{green}\u{2713}{reset}"),
                    format!("{dim}\u{2717}{reset}"),
                    format!("{dim}\u{2717}{reset}"),
                ),
            };
            let level_col = format!("{bold}P{reset}{p1}{p2}{p3}");

            let const_info = sig.const_param_info();
            let params_desc = if const_info.is_empty() {
                String::new()
            } else {
                let p: Vec<String> = const_info
                    .iter()
                    .map(|(name, required)| {
                        if *required {
                            name.to_string()
                        } else {
                            format!("[{name}]")
                        }
                    })
                    .collect();
                format!("({})", p.join(", "))
            };

            let arity = if sig.outputs == 0 {
                format!("{}→N", sig.wire_input_count())
            } else {
                format!("{}→{}", sig.wire_input_count(), sig.outputs)
            };

            let name_padded = format!("{:<24}", sig.name);
            let params_padded = format!("{:<24}", params_desc);
            let arity_padded = format!("{:<5}", arity);

            print!("  {bold}{magenta}{name_padded}{reset}");
            print!(" {dim}{params_padded}{reset}");
            print!(" {arity_padded}");
            print!("  {level_col}");
            println!("  {dim}{}{reset}", sig.description);

            if verbose {
                // Extra line: polydat module-definition syntax
                // form of the function — `name(arg: type, ...) ->
                // (out: type, ...)` — plus the
                // commutativity/associativity tag and arity
                // qualifier. This is the same surface the
                // workload-author sees in the polydat manual,
                // so workload bindings and describe output stay
                // word-for-word aligned.
                let module_def_line = format_module_def_signature(sig);
                let assoc_line = format_commutativity_and_arity(sig);
                println!("  {dim}    sig: {reset}{module_def_line}");
                if !assoc_line.is_empty() {
                    println!("  {dim}    attr:{reset} {dim}{assoc_line}{reset}");
                }
            }
        }
        println!();
    }

    println!(
        "  {bold}Legend:{reset}  {bold}P{reset}{green}\u{2713}{reset}{green}\u{2713}{reset}{green}\u{2713}{reset} = supported levels  {green}\u{2713}{reset} = yes  {dim}\u{2717}{reset} = no"
    );
    println!("    {bold}P{reset}3  Cranelift native code       {dim}(~0.2ns/node){reset}");
    println!("    {bold}P{reset}2  Compiled u64 closure        {dim}(~4.5ns/node){reset}");
    println!("    {bold}P{reset}1  Runtime Value interpreter   {dim}(~70ns/node){reset}");
    println!();
    println!("  {dim}Levels probed from live node instances.{reset}");
    println!("  {dim}Nodes with constant params (mod, div, etc.) reach P3 when{reset}");
    println!("  {dim}constants are known at assembly time, P2 otherwise.{reset}");
    if !verbose {
        println!();
        println!(
            "  {dim}Pass --verbose for per-function signatures, types, and associativity.{reset}"
        );
    }
    println!();
}

/// Render a `FuncSig` as a polydat module-definition signature:
/// `name(arg1: type1, arg2: type2, ...) -> (out: type)`.
///
/// Wire input types and output types are probed from the live
/// node — the same probe `probe_compile_level` uses to fold a
/// `out := func(args)` program, then read `NodeMeta` off the
/// resulting node. Falls back to `?` only when the probe fails
/// (rare; signature was unrenderable or compile error).
///
/// For variadic functions an `...` ellipsis follows the
/// last repeating slot.
fn format_module_def_signature(sig: &polydat::dsl::registry::FuncSig) -> String {
    use polydat::dsl::registry::Arity;
    // Probe the live node's NodeMeta to get real wire input
    // types + real output port types.
    let probed = probe_node_meta(sig.name, sig.params);
    let wire_types: Vec<String> = probed
        .as_ref()
        .map(|m| {
            m.wire_inputs()
                .iter()
                .map(|p| port_type_label(p.typ).to_string())
                .collect()
        })
        .unwrap_or_default();
    let out_types: Vec<String> = probed
        .as_ref()
        .map(|m| {
            m.outs
                .iter()
                .map(|p| port_type_label(p.typ).to_string())
                .collect()
        })
        .unwrap_or_default();

    let mut parts: Vec<String> = Vec::with_capacity(sig.params.len());
    let mut wire_idx: usize = 0;
    for p in sig.params {
        let ty = if p.slot_type.is_wire() {
            let resolved = wire_types
                .get(wire_idx)
                .cloned()
                .unwrap_or_else(|| "wire".to_string());
            wire_idx += 1;
            resolved
        } else {
            slot_type_label(p.slot_type).to_string()
        };
        let prefix = if p.required { "" } else { "[" };
        let suffix = if p.required { "" } else { "]" };
        parts.push(format!("{prefix}{name}: {ty}{suffix}", name = p.name));
    }
    let arity_suffix = match &sig.arity {
        Arity::Fixed => "",
        Arity::VariadicWires { .. }
        | Arity::VariadicConsts { .. }
        | Arity::VariadicGroup { .. } => ", ...",
    };
    let args = format!("{}{arity_suffix}", parts.join(", "));
    let out = if out_types.is_empty() {
        if sig.outputs == 0 {
            "(out: ?, ...)".to_string()
        } else if sig.outputs == 1 {
            "(out: ?)".to_string()
        } else {
            let outs: Vec<String> = (0..sig.outputs).map(|i| format!("out{i}: ?")).collect();
            format!("({})", outs.join(", "))
        }
    } else if out_types.len() == 1 {
        format!("(out: {})", out_types[0])
    } else {
        let outs: Vec<String> = out_types
            .iter()
            .enumerate()
            .map(|(i, t)| format!("out{i}: {t}"))
            .collect();
        format!("({})", outs.join(", "))
    };
    format!("{name}({args}) -> {out}", name = sig.name)
}

/// Probe the live NodeMeta for a function by compiling
/// `out := func(args)` with example args and reading the last
/// node's meta. Returns `None` if the compile fails — the
/// caller falls back to `wire` / `?` placeholders.
fn probe_node_meta(
    func_name: &str,
    params: &[polydat::dsl::registry::ParamSpec],
) -> Option<polydat::ast::NodeMeta> {
    use std::collections::BTreeSet;
    let parts: Vec<String> = params.iter().map(|p| p.example.to_string()).collect();
    // Collect every wire-slot example so the synthesized source
    // can declare them as inputs. Without this, functions whose
    // wire example isn't `cycle` (e.g. `limit(input: wire,
    // example="row")`) fail the probe with "unknown identifier
    // `row`" and the verbose surface falls back to "wire"/"?"
    // placeholders.
    let mut wire_examples: BTreeSet<&str> = BTreeSet::new();
    wire_examples.insert("cycle");
    for p in params {
        if p.slot_type.is_wire() {
            wire_examples.insert(p.example);
        }
    }
    let input_decls: String = wire_examples
        .iter()
        .map(|name| format!("input {name}: u64\n"))
        .collect();
    let source = if parts.is_empty() {
        format!("{input_decls}out := {func_name}()")
    } else {
        format!("{input_decls}out := {func_name}({})", parts.join(", "))
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        polydat::dsl::compile_polydat(&source)
    }));
    match result {
        Ok(Ok(kernel)) => {
            let program = kernel.program();
            // Find the node whose name matches `func_name`,
            // walking in reverse so the most-recent matching
            // one wins. The compiler may inject helpers
            // (adapter splices, const folds) ahead of or
            // behind the user-visible node; matching by name
            // skips them.
            let n = program.node_count();
            for i in (0..n).rev() {
                let m = program.node_meta(i);
                if m.name == func_name {
                    return Some(polydat::ast::NodeMeta {
                        name: m.name.clone(),
                        ins: m.ins.clone(),
                        outs: m.outs.clone(),
                    });
                }
            }
            None
        }
        _ => None,
    }
}

fn port_type_label(t: polydat::ast::PortType) -> &'static str {
    use polydat::ast::PortType;
    match t {
        PortType::U64 => "u64",
        PortType::F64 => "f64",
        PortType::U32 => "u32",
        PortType::I32 => "i32",
        PortType::I64 => "i64",
        PortType::F32 => "f32",
        PortType::U8 => "u8",
        PortType::I8 => "i8",
        PortType::U16 => "u16",
        PortType::I16 => "i16",
        PortType::F16 => "f16",
        PortType::U128 => "u128",
        PortType::I128 => "i128",
        PortType::Reg128 => "reg128",
        PortType::RegI8x16 => "reg<i8x16>",
        PortType::RegI16x8 => "reg<i16x8>",
        PortType::RegI32x4 => "reg<i32x4>",
        PortType::RegI64x2 => "reg<i64x2>",
        PortType::RegF16x8 => "reg<f16x8>",
        PortType::RegF32x4 => "reg<f32x4>",
        PortType::RegF64x2 => "reg<f64x2>",
        PortType::Bool => "bool",
        PortType::Str => "str",
        PortType::Bytes => "bytes",
        PortType::Json => "json",
        PortType::Ext => "ext",
        PortType::Handle => "handle",
        PortType::VecF32 => "vec<f32>",
        PortType::VecI32 => "vec<i32>",
        PortType::VecF64 => "vec<f64>",
        PortType::VecI64 => "vec<i64>",
        PortType::VecF16 => "vec<f16>",
        PortType::VecI16 => "vec<i16>",
        PortType::VecI8 => "vec<i8>",
    }
}

fn slot_type_label(slot: polydat::ast::SlotType) -> &'static str {
    use polydat::ast::SlotType;
    match slot {
        SlotType::Wire => "wire",
        SlotType::ConstU64 => "const u64",
        SlotType::ConstF64 => "const f64",
        SlotType::ConstStr => "const str",
        SlotType::ConstVecU64 => "const [u64]",
        SlotType::ConstVecF64 => "const [f64]",
        SlotType::ConstVec => "const [T]",
    }
}

fn format_commutativity_and_arity(sig: &polydat::dsl::registry::FuncSig) -> String {
    use polydat::ast::Commutativity;
    use polydat::dsl::registry::Arity;
    let mut bits: Vec<String> = Vec::new();
    match &sig.commutativity {
        Commutativity::Positional => bits.push("positional".into()),
        Commutativity::AllCommutative => bits.push("commutative".into()),
        Commutativity::Groups(g) => bits.push(format!("commute-groups={g:?}")),
    }
    match &sig.arity {
        Arity::Fixed => bits.push(format!("arity=fixed({})", sig.params.len())),
        Arity::VariadicWires { min_wires } => {
            bits.push(format!("arity=variadic-wires(min={min_wires})"))
        }
        Arity::VariadicConsts { min_consts } => {
            bits.push(format!("arity=variadic-consts(min={min_consts})"))
        }
        Arity::VariadicGroup { group, min_repeats } => bits.push(format!(
            "arity=variadic-group({n} types × min {min_repeats})",
            n = group.len()
        )),
    }
    if sig.outputs == 0 {
        bits.push("dynamic outputs".into());
    }
    bits.join("  ");
    bits.join("  ")
}

/// Dump all Polydat node function metadata to a markdown file.
///
/// Writes a complete reference of all registered functions grouped
/// by category, including signatures, parameters, descriptions,
/// and help text.
fn describe_wiring_functions_md(path: &str) {
    use nmbrs_runtime::bindings::probe_compile_level;
    use std::io::Write;

    let grouped = registry::by_category();
    let mut out = String::new();

    out.push_str("# Polydat Node Functions Reference\n\n");
    out.push_str("Auto-generated by `nmbrs describe wiring functions-md`.\n\n");

    // Summary table
    let total: usize = grouped.iter().map(|(_, funcs)| funcs.len()).sum();
    out.push_str(&format!(
        "**{total} functions** across {} categories.\n\n",
        grouped.len()
    ));

    out.push_str("## Table of Contents\n\n");
    for (cat, funcs) in &grouped {
        let anchor = cat.display_name().to_lowercase().replace(' ', "-");
        out.push_str(&format!("- [{}](#{})", cat.display_name(), anchor));
        out.push_str(&format!(" ({} functions)\n", funcs.len()));
    }
    out.push('\n');

    for (cat, funcs) in &grouped {
        out.push_str(&format!("## {}\n\n", cat.display_name()));

        // Summary table for this category. Rows are collected first so the
        // renderer can pad every cell to its column width — emitting a fixed
        // `|----------|` divider beside unpadded cells left the grid ragged.
        let mut table_rows: Vec<Vec<String>> = Vec::new();

        for sig in funcs {
            let level = probe_compile_level(sig.name);
            let jit = match level {
                registry::CompileLevel::Phase3 => "P3",
                registry::CompileLevel::Phase2 => "P2",
                registry::CompileLevel::Phase1 => "P1",
            };

            let const_info = sig.const_param_info();
            let params_desc = if const_info.is_empty() {
                String::new()
            } else {
                let p: Vec<String> = const_info
                    .iter()
                    .map(|(name, required)| {
                        if *required {
                            name.to_string()
                        } else {
                            format!("[{name}]")
                        }
                    })
                    .collect();
                format!("({})", p.join(", "))
            };

            let arity = if sig.outputs == 0 {
                format!("{}→N", sig.wire_input_count())
            } else {
                format!("{}→{}", sig.wire_input_count(), sig.outputs)
            };

            // Escape pipes in description
            let desc = sig.description.replace('|', "\\|");
            table_rows.push(vec![
                format!("`{}`", sig.name),
                params_desc,
                arity,
                jit.to_string(),
                desc,
            ]);
        }
        let headers: Vec<String> = ["Function", "Params", "Arity", "JIT", "Description"]
            .iter()
            .map(|h| (*h).to_string())
            .collect();
        // All columns left-aligned: every one is prose or a short token, none
        // numeric.
        out.push_str(&crate::report::markdown_table(
            &headers,
            &table_rows,
            headers.len(),
        ));
        out.push('\n');

        // Detailed entries with help text
        for sig in funcs {
            out.push_str(&format!("### `{}`\n\n", sig.name));

            // Build full signature
            let mut all_params: Vec<String> = Vec::new();
            for p in sig.params {
                match p.slot_type {
                    polydat::ast::SlotType::Wire => {
                        all_params.push(format!("{}: wire", p.name));
                    }
                    polydat::ast::SlotType::ConstStr => {
                        if p.required {
                            all_params.push(format!("{}: str", p.name));
                        } else {
                            all_params.push(format!("[{}]: str", p.name));
                        }
                    }
                    polydat::ast::SlotType::ConstU64 => {
                        if p.required {
                            all_params.push(format!("{}: u64", p.name));
                        } else {
                            all_params.push(format!("[{}]: u64", p.name));
                        }
                    }
                    polydat::ast::SlotType::ConstF64 => {
                        if p.required {
                            all_params.push(format!("{}: f64", p.name));
                        } else {
                            all_params.push(format!("[{}]: f64", p.name));
                        }
                    }
                    polydat::ast::SlotType::ConstVecU64 => {
                        all_params.push(format!("{}: vec<u64>", p.name));
                    }
                    polydat::ast::SlotType::ConstVecF64 => {
                        all_params.push(format!("{}: vec<f64>", p.name));
                    }
                    polydat::ast::SlotType::ConstVec => {
                        all_params.push(format!("{}: vec<T>", p.name));
                    }
                }
            }
            let sig_str = format!("{}({}) → {}", sig.name, all_params.join(", "), sig.outputs);
            out.push_str(&format!("**Signature:** `{sig_str}`\n\n"));
            out.push_str(&format!(
                "**Category:** {}  \n",
                sig.category.display_name()
            ));

            let level = probe_compile_level(sig.name);
            let jit = match level {
                registry::CompileLevel::Phase3 => "P3 (Cranelift native)",
                registry::CompileLevel::Phase2 => "P2 (compiled u64 closure)",
                registry::CompileLevel::Phase1 => "P1 (Value interpreter)",
            };
            out.push_str(&format!("**JIT Level:** {jit}  \n"));

            if sig.is_variadic() {
                out.push_str("**Variadic:** yes  \n");
            }

            out.push_str(&format!("\n{}\n\n", sig.description));

            if !sig.help.is_empty() {
                out.push_str("```\n");
                out.push_str(sig.help);
                out.push_str("\n```\n\n");
            }

            out.push_str("---\n\n");
        }
    }

    let mut f = std::fs::File::create(path).unwrap_or_else(|e| {
        eprintln!("error: failed to create {path}: {e}");
        std::process::exit(1);
    });
    f.write_all(out.as_bytes()).unwrap_or_else(|e| {
        eprintln!("error: failed to write {path}: {e}");
        std::process::exit(1);
    });
    eprintln!("nmbrs: wrote {total} functions to {path}");
}

/// Display embedded stdlib modules with their typed signatures.
///
/// Render the catalog of wiring port types with their canonical
/// names + per-variant docstrings. Mirrors the layout of
/// `describe_wiring_functions` (header, dimmed descriptions) so
/// the two surfaces look like sibling readouts.
fn describe_wiring_types() {
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let (bold, dim, reset, cyan, magenta) = if is_tty {
        ("\x1b[1m", "\x1b[2m", "\x1b[0m", "\x1b[36m", "\x1b[35m")
    } else {
        ("", "", "", "", "")
    };

    println!();
    println!("{bold}Wiring Port Types{reset}");
    println!("{bold}═════════════════{reset}");
    println!();

    let groups: &[(&str, &[(polydat::ast::PortType, &str)])] = &[
        (
            "Scalars (numeric)",
            &[
                (
                    polydat::ast::PortType::U64,
                    "64-bit unsigned integer; the primary numeric carrier",
                ),
                (polydat::ast::PortType::I64, "64-bit signed integer"),
                (
                    polydat::ast::PortType::U32,
                    "32-bit unsigned integer; widens to U64 automatically",
                ),
                (
                    polydat::ast::PortType::I32,
                    "32-bit signed integer; widens to I64 automatically",
                ),
                (
                    polydat::ast::PortType::F64,
                    "64-bit IEEE 754 float (math, distributions, noise)",
                ),
                (
                    polydat::ast::PortType::F32,
                    "32-bit IEEE 754 float; widens to F64 automatically",
                ),
            ],
        ),
        (
            "Scalars (other)",
            &[
                (
                    polydat::ast::PortType::Bool,
                    "Boolean true/false; widens to U64 (1/0)",
                ),
                (
                    polydat::ast::PortType::Str,
                    "Heap-allocated string; everything auto-converts to Str",
                ),
                (polydat::ast::PortType::Bytes, "Raw byte buffer"),
                (polydat::ast::PortType::Json, "Structured JSON value"),
            ],
        ),
        (
            "Vectors",
            &[
                (
                    polydat::ast::PortType::VecF32,
                    "Typed `f32` vector (`Arc<[f32]>`); native for CQL `vector<float, N>`",
                ),
                (
                    polydat::ast::PortType::VecF64,
                    "Typed `f64` vector (`Arc<[f64]>`); native for CQL `vector<double, N>`",
                ),
                (
                    polydat::ast::PortType::VecF16,
                    "Typed half-precision float vector (`Arc<[half::f16]>`); native for CQL `vector<half_float, N>`",
                ),
                (
                    polydat::ast::PortType::VecI16,
                    "Typed `i16` vector (`Arc<[i16]>`); native for CQL `vector<smallint, N>`",
                ),
                (
                    polydat::ast::PortType::VecI32,
                    "Typed `i32` vector (`Arc<[i32]>`)",
                ),
                (
                    polydat::ast::PortType::VecI64,
                    "Typed `i64` vector (`Arc<[i64]>`); native for CQL `vector<bigint, N>`",
                ),
            ],
        ),
        (
            "Reference types",
            &[
                (
                    polydat::ast::PortType::Handle,
                    "Type-erased `Arc<dyn Any + Send + Sync>` handle to a resolved resource (dataset, prepared statement, ...)",
                ),
                (
                    polydat::ast::PortType::Ext,
                    "Adapter-contributed reflected type (e.g. CQL UUID)",
                ),
            ],
        ),
    ];

    for (group_name, types) in groups {
        println!("  {bold}{cyan}── {group_name} ──{reset}");
        println!();
        for (t, desc) in *types {
            let label = port_type_label(*t);
            let label_padded = format!("{:<16}", label);
            println!("  {bold}{magenta}{label_padded}{reset}  {dim}{desc}{reset}");
        }
        println!();
    }
}

/// Dump the wiring port types as a markdown reference table.
fn describe_wiring_types_md(path: &str) {
    use std::io::Write;
    let mut out = String::new();
    out.push_str("# Wiring Port Types\n\n");
    out.push_str("Auto-generated by `nmbrs describe wiring types-md`.\n\n");
    out.push_str("Port types are the wire-level type tags carried by every wiring node's input / output ports. Conversions are inserted automatically at compile time when a producer's port type differs from its consumer's; see the widening notes per variant.\n\n");

    let groups: &[(&str, &[(polydat::ast::PortType, &str)])] = &[
        (
            "Scalars (numeric)",
            &[
                (
                    polydat::ast::PortType::U64,
                    "64-bit unsigned integer; the primary numeric carrier",
                ),
                (polydat::ast::PortType::I64, "64-bit signed integer"),
                (
                    polydat::ast::PortType::U32,
                    "32-bit unsigned integer; widens to U64 automatically",
                ),
                (
                    polydat::ast::PortType::I32,
                    "32-bit signed integer; widens to I64 automatically",
                ),
                (
                    polydat::ast::PortType::F64,
                    "64-bit IEEE 754 float (math, distributions, noise)",
                ),
                (
                    polydat::ast::PortType::F32,
                    "32-bit IEEE 754 float; widens to F64 automatically",
                ),
            ],
        ),
        (
            "Scalars (other)",
            &[
                (
                    polydat::ast::PortType::Bool,
                    "Boolean true/false; widens to U64 (1/0)",
                ),
                (
                    polydat::ast::PortType::Str,
                    "Heap-allocated string; everything auto-converts to Str",
                ),
                (polydat::ast::PortType::Bytes, "Raw byte buffer"),
                (polydat::ast::PortType::Json, "Structured JSON value"),
            ],
        ),
        (
            "Vectors",
            &[
                (
                    polydat::ast::PortType::VecF32,
                    "Typed `f32` vector (`Arc<[f32]>`); native for CQL `vector<float, N>`",
                ),
                (
                    polydat::ast::PortType::VecF64,
                    "Typed `f64` vector (`Arc<[f64]>`); native for CQL `vector<double, N>`",
                ),
                (
                    polydat::ast::PortType::VecF16,
                    "Typed half-precision float vector (`Arc<[half::f16]>`); native for CQL `vector<half_float, N>`",
                ),
                (
                    polydat::ast::PortType::VecI16,
                    "Typed `i16` vector (`Arc<[i16]>`); native for CQL `vector<smallint, N>`",
                ),
                (
                    polydat::ast::PortType::VecI32,
                    "Typed `i32` vector (`Arc<[i32]>`)",
                ),
                (
                    polydat::ast::PortType::VecI64,
                    "Typed `i64` vector (`Arc<[i64]>`); native for CQL `vector<bigint, N>`",
                ),
            ],
        ),
        (
            "Reference types",
            &[
                (
                    polydat::ast::PortType::Handle,
                    "Type-erased `Arc<dyn Any + Send + Sync>` handle to a resolved resource (dataset, prepared statement, ...)",
                ),
                (
                    polydat::ast::PortType::Ext,
                    "Adapter-contributed reflected type (e.g. CQL UUID)",
                ),
            ],
        ),
    ];

    for (group_name, types) in groups {
        out.push_str(&format!("## {group_name}\n\n"));
        let headers: Vec<String> = vec!["Label".to_string(), "Description".to_string()];
        let rows: Vec<Vec<String>> = types
            .iter()
            .map(|(t, desc)| vec![format!("`{}`", port_type_label(*t)), (*desc).to_string()])
            .collect();
        out.push_str(&crate::report::markdown_table(
            &headers,
            &rows,
            headers.len(),
        ));
        out.push('\n');
    }

    match std::fs::File::create(path).and_then(|mut f| f.write_all(out.as_bytes())) {
        Ok(_) => println!("wrote {path} ({} bytes)", out.len()),
        Err(e) => eprintln!("failed to write {path}: {e}"),
    }
}

/// Parses each `.polydat` source from the compiled-in standard library,
/// extracts `ModuleDef` statements, and prints them grouped by
/// category (source filename) with ANSI coloring.
fn describe_wiring_stdlib() {
    use polydat::dsl::ast::Statement;
    use polydat::dsl::lexer::lex;
    use polydat::dsl::parser::parse;

    let sources = polydat::dsl::stdlib_sources();
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());

    let (bold, dim, reset, green, cyan, magenta) = if is_tty {
        (
            "\x1b[1m", "\x1b[2m", "\x1b[0m", "\x1b[32m", "\x1b[36m", "\x1b[35m",
        )
    } else {
        ("", "", "", "", "", "")
    };

    println!();
    println!("{bold}GK Standard Library{reset}");
    println!("{bold}═══════════════════{reset}");
    println!();

    for (filename, source) in sources {
        // Category name: filename without .polydat extension, title-cased
        let category = filename.strip_suffix(".polydat").unwrap_or(filename);
        let category_title = category
            .chars()
            .enumerate()
            .map(|(i, c)| if i == 0 { c.to_ascii_uppercase() } else { c })
            .collect::<String>();

        let tokens = match lex(source) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("warning: failed to lex stdlib file: {e}");
                continue;
            }
        };
        let ast = match parse(tokens) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("warning: failed to parse stdlib file: {e}");
                continue;
            }
        };

        // Collect module defs from this file
        let mut modules = Vec::new();
        for stmt in &ast.statements {
            if let Statement::ModuleDef(mdef) = stmt {
                modules.push(mdef);
            }
        }

        if modules.is_empty() {
            continue;
        }

        println!("  {bold}{cyan}── {category_title} ──{reset}");
        println!();

        for mdef in &modules {
            // Build typed params string: (name: type, name: type, ...)
            let params_str = mdef
                .params
                .iter()
                .map(|p| format!("{}: {}", p.name, p.typ))
                .collect::<Vec<_>>()
                .join(", ");

            // Build typed outputs string: (name: type, ...)
            let outputs_str = mdef
                .outputs
                .iter()
                .map(|p| format!("{}: {}", p.name, p.typ))
                .collect::<Vec<_>>()
                .join(", ");

            let signature = format!("({params_str}) -> ({outputs_str})");

            // Extract the first comment line immediately before this module def
            // by scanning the source text for the comment block above the def
            let description = extract_first_comment(source, &mdef.name);

            // Name column: bold magenta, padded to 24 chars
            let name_padded = format!("{:<24}", mdef.name);
            print!("  {bold}{magenta}{name_padded}{reset}");

            // Signature in green
            println!(" {green}{signature}{reset}");

            // Description on the next line, indented and dim
            if let Some(desc) = description {
                println!("  {:<24} {dim}{desc}{reset}", "");
            }

            println!();
        }
    }
}

/// Display Polydat modules found in a directory.
///
/// Scans a directory for `.polydat` files, parses each one, extracts
/// `ModuleDef` statements, and displays them with their typed
/// signatures — same format as `describe wiring stdlib`.
///
/// Usage:
///   nmbrs describe wiring modules [--dir=path]
fn describe_wiring_modules(args: &[String]) {
    use polydat::dsl::ast::Statement;
    use polydat::dsl::lexer::lex;
    use polydat::dsl::parser::parse;

    let dir = args
        .iter()
        .find_map(|a| a.strip_prefix("--dir="))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        });

    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());

    let (bold, dim, reset, green, cyan, magenta) = if is_tty {
        (
            "\x1b[1m", "\x1b[2m", "\x1b[0m", "\x1b[32m", "\x1b[36m", "\x1b[35m",
        )
    } else {
        ("", "", "", "", "", "")
    };

    println!();
    println!("{bold}GK Modules in {}{reset}", dir.display());
    println!(
        "{bold}{}{reset}",
        "═".repeat(15 + dir.display().to_string().len())
    );
    println!();

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: cannot read directory '{}': {e}", dir.display());
            return;
        }
    };

    let mut polydat_files: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("polydat"))
        .collect();
    polydat_files.sort();

    if polydat_files.is_empty() {
        println!("  {dim}(no .polydat files found){reset}");
        println!();
        return;
    }

    for path in &polydat_files {
        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("warning: failed to read {}: {e}", path.display());
                continue;
            }
        };

        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let category = filename.strip_suffix(".polydat").unwrap_or(filename);
        let category_title = category
            .chars()
            .enumerate()
            .map(|(i, c)| if i == 0 { c.to_ascii_uppercase() } else { c })
            .collect::<String>();

        let tokens = match lex(&source) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("warning: failed to lex {filename}: {e}");
                continue;
            }
        };
        let ast = match parse(tokens) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("warning: failed to parse {filename}: {e}");
                continue;
            }
        };

        let mut modules = Vec::new();
        for stmt in &ast.statements {
            if let Statement::ModuleDef(mdef) = stmt {
                modules.push(mdef);
            }
        }

        if modules.is_empty() {
            continue;
        }

        println!("  {bold}{cyan}-- {category_title} ({filename}) --{reset}");
        println!();

        for mdef in &modules {
            let params_str = mdef
                .params
                .iter()
                .map(|p| format!("{}: {}", p.name, p.typ))
                .collect::<Vec<_>>()
                .join(", ");

            let outputs_str = mdef
                .outputs
                .iter()
                .map(|p| format!("{}: {}", p.name, p.typ))
                .collect::<Vec<_>>()
                .join(", ");

            let signature = format!("({params_str}) -> ({outputs_str})");

            let description = extract_first_comment(&source, &mdef.name);

            let name_padded = format!("{:<24}", mdef.name);
            print!("  {bold}{magenta}{name_padded}{reset}");
            println!(" {green}{signature}{reset}");

            if let Some(desc) = description {
                println!("  {:<24} {dim}{desc}{reset}", "");
            }

            println!();
        }
    }
}

/// Extract the first comment line above a module definition.
///
/// Scans for `// <text>` lines in the comment block immediately
/// preceding the line that starts with `<name>(`. Only the nearest
/// contiguous comment block is considered — a blank line ends the
/// block. Returns the first non-empty line from that block.
fn extract_first_comment(source: &str, name: &str) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    // Find the line where the module def starts
    let def_prefix = format!("{name}(");
    let def_idx = lines
        .iter()
        .position(|l| l.trim_start().starts_with(&def_prefix))?;

    // Walk backwards from the def line, collecting the nearest comment block.
    // Stop at the first blank line or non-comment line.
    let mut comment_lines = Vec::new();
    let mut i = def_idx;
    let mut seen_comment = false;
    while i > 0 {
        i -= 1;
        let trimmed = lines[i].trim();
        if trimmed.starts_with("//") {
            let text = trimmed.strip_prefix("//").unwrap().trim();
            comment_lines.push(text);
            seen_comment = true;
        } else if trimmed.is_empty() {
            if seen_comment {
                // Blank line after we already found comments — end of block
                break;
            }
            // Blank line directly above def (before any comment) — skip
            continue;
        } else {
            break;
        }
    }

    // comment_lines is in reverse order; flip to get first-to-last
    comment_lines.reverse();
    // Return the first non-empty line
    for line in &comment_lines {
        if !line.is_empty() {
            return Some(line.to_string());
        }
    }
    None
}

/// Render a Polydat source file as DOT, Mermaid, or SVG.
///
/// Usage:
///   nmbrs describe wiring dag <file.polydat> [--format=dot|mermaid|svg] [--output=file]
///   nmbrs describe wiring dag --with-flattening <workload.yaml>
fn describe_wiring_dag(args: &[String]) {
    use polydat::viz;

    let file = args.iter().find(|a| !a.starts_with("--"));
    let format = args
        .iter()
        .find_map(|a| a.strip_prefix("--format="))
        .unwrap_or("dot");
    let output = args.iter().find_map(|a| a.strip_prefix("--output="));
    let with_flattening = args.iter().any(|a| a == "--with-flattening");

    let source = match file {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: failed to read '{}': {e}", path);
                return;
            }
        },
        None => {
            eprintln!(
                "nmbrs describe wiring dag <file.polydat> [--format=dot|mermaid|svg] [--output=file]"
            );
            eprintln!("nmbrs describe wiring dag --with-flattening <workload.yaml>");
            eprintln!();
            eprintln!("Renders a Polydat source file as a DAG diagram.");
            eprintln!("  --format=dot         DOT digraph (default)");
            eprintln!("  --format=mermaid     Mermaid flowchart");
            eprintln!("  --format=svg         Self-contained SVG (pure Rust, no external tools)");
            eprintln!("  --output=file        Write to file instead of stdout");
            eprintln!("  --with-flattening    Treat <file> as a workload YAML; print");
            eprintln!("                       the SRD-13d scope-elision summary");
            eprintln!("                       (materialised bit, logical_name, bind_outer)");
            return;
        }
    };

    // SRD-13d Phase 8: --with-flattening switches the surface from
    // "render a Polydat source string" to "parse a workload YAML, build
    // its scope tree, run mark_scope_elision with the
    // 'materialise everything' stub predicate, and print the
    // per-node summary." When SRD-13d Phase 3 lands and supplies
    // the real predicate, swap it in here — the rest of the pipe
    // stays.
    if with_flattening {
        let path = file.map(|s| s.as_str()).unwrap_or("<missing>");
        let summary = render_elision_summary(&source, path);
        match summary {
            Ok(content) => {
                if let Some(p) = output {
                    match std::fs::write(p, &content) {
                        Ok(()) => eprintln!("wrote {} bytes to {p}", content.len()),
                        Err(e) => eprintln!("error: failed to write '{p}': {e}"),
                    }
                } else {
                    print!("{content}");
                }
            }
            Err(e) => eprintln!("error: {e}"),
        }
        return;
    }

    let result = match format {
        "dot" => viz::polydat_to_dot(&source),
        "mermaid" => viz::polydat_to_mermaid(&source),
        "svg" => viz::polydat_to_svg(&source),
        other => {
            eprintln!("error: unknown format '{other}' (use dot, mermaid, or svg)");
            return;
        }
    };

    match result {
        Ok(content) => {
            if let Some(path) = output {
                match std::fs::write(path, &content) {
                    Ok(()) => eprintln!("wrote {} bytes to {path}", content.len()),
                    Err(e) => eprintln!("error: failed to write '{path}': {e}"),
                }
            } else {
                println!("{content}");
            }
        }
        Err(e) => eprintln!("error: {e}"),
    }
}

/// SRD-13d Phase 8 entry point: parse `yaml_source` as a
/// workload, build its [`ScopeTree`], run
/// [`ScopeTree::mark_scope_elision`] with a stub
/// "materialise everything" predicate, and produce a textual
/// summary listing each node's logical_name, materialised
/// bit, and the nearest_materialised ancestor it would bind
/// to (the SRD-13d "walking parent" reference).
///
/// Today's predicate is a stub — SRD-13d Phase 3 will install
/// the real one (consulting `HasPolydatMatter` + program-hash
/// equivalence). Wiring everything else end-to-end now means
/// that swap is a one-liner when the predicate lands.
///
/// `path` is the file path the user supplied; it's surfaced
/// in the header line so the caller can confirm which file
/// was read.
fn render_elision_summary(yaml_source: &str, path: &str) -> Result<String, String> {
    use nmbrs_runtime::scope_tree::{ScopeKind, ScopeTree};
    use nmbrs_workload::parse::{parse_workload, parse_workload_from_path};
    use std::collections::HashMap;

    // SRD-72: when invoked with a real on-disk file, route
    // through parse_workload_from_path so `extends:` chains
    // resolve. Tests pass synthetic literals with fake paths
    // ("test.yaml", "two.yaml") that don't exist on disk; fall
    // back to the in-memory parser there.
    let path_obj = std::path::Path::new(path);
    let workload = if path_obj.is_file() {
        parse_workload_from_path(path_obj, &HashMap::new())
    } else {
        parse_workload(yaml_source, &HashMap::new())
    }
    .map_err(|e| format!("parse_workload('{path}'): {e}"))?;

    // Pick the scenario the same way the runner does: take the
    // user-named scenario or, if absent, synthesise a default
    // from `phase_order`. We don't accept a `--scenario=` knob
    // here today — the diagnostic surface is structural, not
    // configurable.
    let scenario_name = "default";
    let scenario_nodes: Vec<_> = if let Some(nodes) = workload.scenarios.get(scenario_name) {
        nodes.clone()
    } else if !workload.phase_order.is_empty() {
        workload
            .phase_order
            .iter()
            .map(|n| nmbrs_workload::model::ScenarioNode::Phase(n.clone()))
            .collect()
    } else {
        return Err(format!(
            "workload '{path}' has neither a 'default' scenario nor any phases"
        ));
    };

    let mut tree = ScopeTree::build(scenario_name, &scenario_nodes);
    // SRD-13d Phase 3 stub: every node materialises. Swap in
    // the real predicate (HasPolydatMatter classification +
    // program-hash equivalence) when Phase 3 lands.
    tree.mark_scope_elision(|_kind, _idx| true);

    let mut out = String::new();
    out.push_str(&format!("# scope elision summary: {path}\n"));
    out.push_str(&format!("# scenario: {scenario_name}\n"));
    out.push_str("# predicate: stub (materialise everything) — SRD-13d Phase 3 pending\n");
    out.push('\n');
    out.push_str(&format!(
        "{:<5} {:<6} {:<14} {:<50} {:<50} {}\n",
        "idx", "depth", "materialised", "logical_name", "kind", "bind_outer",
    ));
    out.push_str(&format!(
        "{:<5} {:<6} {:<14} {:<50} {:<50} {}\n",
        "---", "-----", "------------", "------------", "----", "----------",
    ));
    for (idx, node) in tree.iter_dfs() {
        let mat = match node.materialised {
            Some(true) => "true",
            Some(false) => "false",
            None => "?",
        };
        // bind_outer = nearest materialised ancestor, walking
        // *strict* parents when this node is itself flattened.
        // For materialised nodes we surface the same identity
        // (own logical_name) so the summary is self-contained:
        // a reader knows where every node binds at a glance.
        let bind_outer = match node.materialised {
            Some(false) => match node.parent.and_then(|p| tree.nearest_materialised(p)) {
                Some(anc) => tree.nodes[anc].logical_name.clone(),
                None => "<none>".to_string(),
            },
            _ => node.logical_name.clone(),
        };
        let kind_label = match &node.kind {
            ScopeKind::Workload => "workload".to_string(),
            other => other.label(),
        };
        out.push_str(&format!(
            "{:<5} {:<6} {:<14} {:<50} {:<50} {}\n",
            idx, node.depth, mat, node.logical_name, kind_label, bind_outer,
        ));
    }
    Ok(out)
}

// ── cli_spec entry ─────────────────────────────────────────

/// `nmbrs describe <topic> …` — every topic is declared as a
/// real subcommand here so a single spec drives both dispatch
/// (the walker matches the topic, calls the leaf's handler)
/// and completion (the cli_spec→completion adapter sees the
/// same subcommand tree). The leaf handlers delegate to the
/// existing `describe_*` functions; their internal argv
/// parsers continue to handle topic-specific flags and
/// positionals (raw_args=true on the leaves so the legacy
/// per-topic parsers don't lose access to their argv).
pub fn spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle_default(p: ParsedCommand) -> Result<(), String> {
        // `nmbrs describe` with no recognized subcommand. Forward any leftover
        // tokens — the walker collects them as positionals — so the
        // `topic=value` shorthand (`describe adapter=cql`) reaches
        // `describe_command`'s `=`-handler instead of being dropped. With no
        // leftovers this renders the topic list (the historical default).
        describe_command(&p.positionals);
        Ok(())
    }
    Command {
        name: "describe",
        help: "Documentation surface (`describe wiring`, `describe adapter`, …).",
        category: Category::Documentation,
        level: Level::FullSurface,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        subcommands: vec![
            adapter_spec(),
            controls_spec(),
            optimizers_spec(),
            wiring_spec(),
            workloads_spec(),
            wrappers_spec(),
            op_spec(),
        ],
        handler: Some(Handler::Sync(handle_default)),
        raw_args: false,
        completion_override: None,
    }
}

fn adapter_spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle(p: ParsedCommand) -> Result<(), String> {
        let mut argv = vec!["adapter".to_string()];
        argv.extend(p.raw.iter().cloned());
        describe_command(&argv);
        Ok(())
    }
    Command {
        name: "adapter",
        help: "List adapters or show one adapter's params + drivers.",
        category: Category::Documentation,
        level: Level::FullSurface,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: vec![crate::cli_spec::Positional {
            name: "adapter",
            help: "Adapter to describe.",
            kind: crate::cli_spec::PositionalKind::ZeroOrOne,
            value: crate::cli_spec::ValueProvider::Custom(
                crate::completion::adapter_names_provider,
            ),
        }],
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: true,
        completion_override: None,
    }
}

fn controls_spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle(p: ParsedCommand) -> Result<(), String> {
        let mut argv = vec!["controls".to_string()];
        argv.extend(p.raw.iter().cloned());
        describe_command(&argv);
        Ok(())
    }
    Command {
        name: "controls",
        help: "List every dynamic control (SRD-23) the binary can declare, or detail one.",
        category: Category::Documentation,
        level: Level::FullSurface,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: vec![crate::cli_spec::Positional {
            name: "control",
            help: "Control to describe.",
            kind: crate::cli_spec::PositionalKind::ZeroOrOne,
            value: crate::cli_spec::ValueProvider::Custom(
                crate::completion::control_names_provider,
            ),
        }],
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: true,
        completion_override: None,
    }
}

fn optimizers_spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle(p: ParsedCommand) -> Result<(), String> {
        let mut argv = vec!["optimizers".to_string()];
        argv.extend(p.raw.iter().cloned());
        describe_command(&argv);
        Ok(())
    }
    Command {
        name: "optimizers",
        help: "List the optimizers registered in this binary, or show one's markdown docs.",
        category: Category::Documentation,
        level: Level::FullSurface,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: vec![crate::cli_spec::Positional {
            name: "optimizer",
            help: "Optimizer to describe.",
            kind: crate::cli_spec::PositionalKind::ZeroOrOne,
            value: crate::cli_spec::ValueProvider::None,
        }],
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: true,
        completion_override: None,
    }
}

fn workloads_spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle(p: ParsedCommand) -> Result<(), String> {
        let mut argv = vec!["workloads".to_string()];
        argv.extend(p.raw.iter().cloned());
        describe_command(&argv);
        Ok(())
    }
    Command {
        name: "workloads",
        help: "List bundled workloads (curated tier; --all / examples / --json), or show one in detail.",
        category: Category::Documentation,
        level: Level::FullSurface,
        flags: vec![
            crate::cli_spec::Flag {
                long: "--all",
                short: None,
                aliases: &[],
                arity: crate::cli_spec::Arity::Bool,
                value: crate::cli_spec::ValueProvider::None,
                help: "Include the examples tier in the listing.",
                repeatable: false,
            },
            crate::cli_spec::Flag {
                long: "--json",
                short: None,
                aliases: &[],
                arity: crate::cli_spec::Arity::Bool,
                value: crate::cli_spec::ValueProvider::None,
                help: "Machine-readable listing.",
                repeatable: false,
            },
        ],
        kv_params: &[],
        dynamic_options: None,
        positionals: vec![crate::cli_spec::Positional {
            name: "name",
            help: "Catalog name for the detail view, or `examples`.",
            kind: crate::cli_spec::PositionalKind::ZeroOrOne,
            value: crate::cli_spec::ValueProvider::Custom(
                crate::completion::describe_workloads_arg_provider,
            ),
        }],
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: true,
        completion_override: None,
    }
}

fn wiring_spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle_wiring_default(_p: ParsedCommand) -> Result<(), String> {
        describe_command(&["wiring".to_string()]);
        Ok(())
    }
    Command {
        name: "wiring",
        help: "Wiring (graph-kernel) topics: functions, types, stdlib, dag, modules.",
        category: Category::Documentation,
        level: Level::FullSurface,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        subcommands: vec![
            wiring_leaf(
                "functions",
                "List wiring functions ([--verbose] adds signatures + types).",
            ),
            wiring_leaf(
                "functions-md",
                "Dump all wiring functions to a markdown file ([<path>] default wiring_functions.md).",
            ),
            wiring_leaf("stdlib", "List embedded standard-library modules."),
            wiring_leaf("types", "List wiring port types with descriptions."),
            wiring_leaf(
                "types-md",
                "Dump wiring types to a markdown file ([<path>] default wiring_types.md).",
            ),
            wiring_leaf("dag", "Render a wiring source as DOT, Mermaid, or SVG."),
            wiring_leaf("modules", "List modules from a directory."),
        ],
        handler: Some(Handler::Sync(handle_wiring_default)),
        raw_args: false,
        completion_override: None,
    }
}

fn wiring_leaf(name: &'static str, help: &'static str) -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    let leaf_name = name;
    // Use a single shared handler that recovers the leaf name
    // from `p.path` — the walker pushes every matched segment
    // there, so the last element is the wiring leaf's own name.
    fn handle(p: ParsedCommand) -> Result<(), String> {
        let leaf = p.path.last().cloned().unwrap_or_default();
        let mut argv = vec!["wiring".to_string(), leaf];
        argv.extend(p.raw.iter().cloned());
        describe_command(&argv);
        Ok(())
    }
    Command {
        name: leaf_name,
        help,
        category: Category::Documentation,
        level: Level::FullSurface,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: true,
        completion_override: None,
    }
}

fn wrappers_spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle(_p: ParsedCommand) -> Result<(), String> {
        describe_command(&["wrappers".to_string()]);
        Ok(())
    }
    Command {
        name: "wrappers",
        help: "List the registered op-template wrappers.",
        category: Category::Documentation,
        level: Level::FullSurface,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: false,
        completion_override: None,
    }
}

fn op_spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle(p: ParsedCommand) -> Result<(), String> {
        let mut argv = vec!["op".to_string()];
        argv.extend(p.raw.iter().cloned());
        describe_command(&argv);
        Ok(())
    }
    Command {
        name: "op",
        help: "Show the resolved wrapper stack for one op (`describe op <workload> <op>`).",
        category: Category::Documentation,
        level: Level::FullSurface,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: vec![crate::cli_spec::Positional {
            name: "workload",
            help: "Workload file or catalog name.",
            kind: crate::cli_spec::PositionalKind::One,
            value: crate::completion::WORKLOAD_VALUE,
        }],
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: true,
        completion_override: None,
    }
}

// ── SRD-32a Push 4 — wrapper discoverability ─────────────────

/// Render the `nmbrs describe wrappers` table.
///
/// Pulls every registration from the live wrapper inventory and
/// prints one row per wrapper with the columns NAME, OWNED FIELDS,
/// TRIGGER, CONSTRAINTS. The "RANK" column from the SRD draft was
/// dropped on purpose — wrapper composition is constraint-driven
/// now, so rank would be misleading. The trigger column shows a
/// human label ("always", "delay set", "verify/relevancy", …) the
/// caller can grep for; the constraints column is empty for most
/// wrappers and lists `requires_inner=`, `forbids_outer=`, and
/// `mutually_exclusive_with=` only when the wrapper declares them.
///
/// Returned as a String (rather than printed directly) so the
/// test suite can pin the exact output. Iteration order is the
/// alphabetical order the registry hands us — stable across runs.
pub fn render_wrappers_table() -> String {
    use nmbrs_runtime::wrapper_registry::WrapperRegistry;
    use std::fmt::Write;

    let registry = WrapperRegistry::from_inventory();

    // Build rows first so we can compute column widths once.
    struct Row {
        name: String,
        owned: String,
        trigger: String,
        constraints: String,
    }
    let mut rows: Vec<Row> = Vec::with_capacity(registry.len());
    for reg in registry.iter() {
        let owned = if reg.owned_fields.is_empty() {
            "(none)".to_string()
        } else {
            reg.owned_fields.join(", ")
        };
        let trigger = trigger_label(reg.name.as_str(), reg.owned_fields);
        let mut constraint_parts: Vec<String> = Vec::new();
        if !reg.requires_inner.is_empty() {
            let names: Vec<&str> = reg.requires_inner.iter().map(|n| n.as_str()).collect();
            constraint_parts.push(format!("requires_inner=[{}]", names.join(", ")));
        }
        if !reg.forbids_outer.is_empty() {
            let names: Vec<&str> = reg.forbids_outer.iter().map(|n| n.as_str()).collect();
            constraint_parts.push(format!("forbids_outer=[{}]", names.join(", ")));
        }
        if !reg.mutually_exclusive_with.is_empty() {
            let names: Vec<&str> = reg
                .mutually_exclusive_with
                .iter()
                .map(|n| n.as_str())
                .collect();
            constraint_parts.push(format!("mutually_exclusive_with=[{}]", names.join(", "),));
        }
        rows.push(Row {
            name: reg.name.as_str().to_string(),
            owned,
            trigger,
            constraints: constraint_parts.join("; "),
        });
    }

    let name_w = "NAME"
        .len()
        .max(rows.iter().map(|r| r.name.len()).max().unwrap_or(0));
    let owned_w = "OWNED FIELDS"
        .len()
        .max(rows.iter().map(|r| r.owned.len()).max().unwrap_or(0));
    let trigger_w = "TRIGGER"
        .len()
        .max(rows.iter().map(|r| r.trigger.len()).max().unwrap_or(0));

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<name_w$}  {:<owned_w$}  {:<trigger_w$}  CONSTRAINTS",
        "NAME",
        "OWNED FIELDS",
        "TRIGGER",
        name_w = name_w,
        owned_w = owned_w,
        trigger_w = trigger_w,
    );
    for r in &rows {
        let _ = writeln!(
            out,
            "{:<name_w$}  {:<owned_w$}  {:<trigger_w$}  {}",
            r.name,
            r.owned,
            r.trigger,
            r.constraints,
            name_w = name_w,
            owned_w = owned_w,
            trigger_w = trigger_w,
        );
    }
    out
}

/// Human-readable label for a wrapper's trigger predicate.
///
/// The registration carries a `fn(&ParsedOp) -> bool`, which
/// tells us *whether* a wrapper applies but not *what shape of
/// op* drives it. The label is hand-curated per wrapper so the
/// table reads like the SRD's prose. Falls back to
/// "owned field set" for any future wrapper not enumerated here.
fn trigger_label(name: &str, owned_fields: &[&str]) -> String {
    match name {
        "traverse" => "always".to_string(),
        "delay" => "delay set".to_string(),
        "validate" => "verify/relevancy set".to_string(),
        "poll" => "poll: set".to_string(),
        "if" => "if: set".to_string(),
        "fields" => "fields: true".to_string(),
        "result" => "always (no-op when result map empty)".to_string(),
        "metrics" => "non-empty metrics map".to_string(),
        _ if owned_fields.is_empty() => "always".to_string(),
        _ => format!("any of: {}", owned_fields.join(", ")),
    }
}

/// Render the `nmbrs describe op <workload> <op>` text.
///
/// Loads `workload_path` via `nmbrs_workload::parse::parse_workload`
/// (the same idiom as `report_cmd::resolve_items`), then walks
/// every phase's `ops` and the top-level `ops` list looking for
/// an op-template whose `name` matches `op_name`. The first match
/// wins; the phase column tells the caller where it came from
/// (`(phase: foo)` or `(top-level ops:)` when found at the
/// workload root).
///
/// Returns the formatted text on success, or a single-line error
/// string suitable for `eprintln!`. Resolver errors (constraint
/// violations) are surfaced via `Display`, not `Debug` — the user
/// shouldn't see Rust's struct-debug for a config diagnostic.
pub fn render_op_description(workload_path: &str, op_name: &str) -> Result<String, String> {
    use nmbrs_runtime::wrapper_registry::WrapperRegistry;
    use nmbrs_runtime::wrapper_resolver::{WrapperActivation, WrapperResolver};
    use nmbrs_workload::model::ParsedOp;
    use std::collections::HashMap;
    use std::fmt::Write;

    let resolved = crate::cli::resolve_workload_path(workload_path)
        .unwrap_or_else(|| workload_path.to_string());
    let path = std::path::PathBuf::from(&resolved);
    if !path.exists() {
        return Err(format!("workload '{resolved}' not found"));
    }
    let workload = nmbrs_workload::parse::parse_workload_from_path(&path, &HashMap::new())
        .map_err(|e| format!("parse workload '{}': {e}", path.display()))?;

    // Collect every (phase_label, &ParsedOp) pair so we can
    // both find the requested op and list candidates in the
    // not-found path. Walk PHASES first — `parse_workload`
    // flattens phase ops into the top-level `workload.ops` list
    // as well, and the phase context is the more useful label
    // for the user (matches the SRD's example output).
    let mut all_ops: Vec<(Option<String>, &ParsedOp)> = Vec::new();
    for phase_name in &workload.phase_order {
        if let Some(phase) = workload.phases.get(phase_name) {
            for op in &phase.ops {
                all_ops.push((Some(phase_name.clone()), op));
            }
        }
    }
    // Pick up any phase that wasn't in phase_order (defensive —
    // parse_workload always populates phase_order, but we don't
    // want to silently drop ops if it ever doesn't).
    for (phase_name, phase) in &workload.phases {
        if !workload.phase_order.contains(phase_name) {
            for op in &phase.ops {
                all_ops.push((Some(phase_name.clone()), op));
            }
        }
    }
    // Top-level ops come last. Skip any that share a name with
    // a phase op already collected — those are the same template
    // and would only confuse the candidate list.
    let phase_op_names: std::collections::HashSet<&str> =
        all_ops.iter().map(|(_, op)| op.name.as_str()).collect();
    for op in &workload.ops {
        if !phase_op_names.contains(op.name.as_str()) {
            all_ops.push((None, op));
        }
    }

    let found = all_ops.iter().find(|(_, op)| op.name == op_name);
    let (phase_label, template) = match found {
        Some(hit) => hit,
        None => {
            // List candidate names so the user sees what's available.
            let mut candidates: Vec<String> = all_ops
                .iter()
                .map(|(p, op)| match p {
                    Some(ph) => format!("  {} (phase: {ph})", op.name),
                    None => format!("  {} (top-level)", op.name),
                })
                .collect();
            candidates.sort();
            candidates.dedup();
            let mut msg = format!(
                "no op template named '{op_name}' in workload '{}'",
                path.display(),
            );
            if !candidates.is_empty() {
                msg.push_str("\navailable op templates:\n");
                msg.push_str(&candidates.join("\n"));
            }
            return Err(msg);
        }
    };

    let registry = WrapperRegistry::from_inventory();
    let resolver = WrapperResolver::with_default_order(&registry).map_err(|e| e.to_string())?;
    let plan = resolver
        .resolve(
            nmbrs_runtime::wrapper_registry::WrapperSubject::Op(*template),
            &registry,
        )
        .map_err(|e| e.to_string())?;

    let mut out = String::new();
    match phase_label {
        Some(ph) => {
            let _ = writeln!(out, "op '{op_name}' (phase: {ph})");
        }
        None => {
            let _ = writeln!(out, "op '{op_name}' (top-level ops:)");
        }
    }
    out.push_str("  wrapper stack (innermost -> outermost):\n");
    for (i, reg) in plan.iter_innermost_first().enumerate() {
        let line = (reg.describe_assignment)(nmbrs_runtime::wrapper_registry::WrapperSubject::Op(
            *template,
        ))
        .unwrap_or_else(|| reg.name.as_str().to_string());
        // Prepend the wrapper name to assignments that don't
        // already start with it — `describe_assignment` lines
        // typically lead with `<name>: …` already, but the
        // `traverse` and similar None-returners don't.
        let display = if line.starts_with(reg.name.as_str()) {
            line
        } else {
            format!("{}: {line}", reg.name.as_str())
        };
        let provenance = match plan.activation(reg.name) {
            Some(WrapperActivation::OwnedField { field, .. }) => {
                format!(" (triggered by `{field}:` field)")
            }
            Some(WrapperActivation::TransitiveFrom { requested_by, .. }) => {
                format!(" (transitive via {requested_by})")
            }
            Some(WrapperActivation::AlwaysOn { .. }) | None => String::new(),
        };
        let _ = writeln!(out, "    {n}. {display}{provenance}", n = i + 1);
    }
    Ok(out)
}

#[cfg(test)]
mod describe_wiring_dag_flattening_tests {
    //! SRD-13d Phase 8 — the `--with-flattening` surface on
    //! `nmbrs describe wiring dag`. Drives the same code path the CLI
    //! does (parse YAML → build ScopeTree → mark → render
    //! summary) and asserts the produced text contains the
    //! per-node fields the SRD calls out: logical_name,
    //! materialised, and the bind_outer reference.
    use super::render_elision_summary;

    /// Minimal flat workload — one phase under the implicit
    /// scenario. Exercises the simplest path: workload, scenario,
    /// phase. With the all-materialise stub every node should be
    /// `materialised=true` and `bind_outer` equal to its own
    /// `logical_name`.
    #[test]
    fn flat_phase_workload_summary_contains_logical_names() {
        let yaml = r#"
phases:
  setup:
    ops:
      - op: noop
"#;
        let out = render_elision_summary(yaml, "test.yaml")
            .expect("flat workload should parse and render");
        // Header sanity.
        assert!(out.contains("# scope elision summary: test.yaml"));
        assert!(out.contains("# scenario: default"));
        // Logical names per SRD-13d §5.3.
        assert!(
            out.contains("workload"),
            "root node logical_name 'workload' missing:\n{out}"
        );
        assert!(
            out.contains("workload.scenario.default"),
            "scenario logical_name missing:\n{out}"
        );
        assert!(
            out.contains("workload.scenario.default.phase.setup"),
            "phase logical_name missing:\n{out}"
        );
        // Stub predicate ⇒ everyone's materialised.
        assert!(
            out.contains("true"),
            "expected at least one materialised=true row:\n{out}"
        );
        // No 'false' rows under the all-materialise stub. (We
        // can't assert "no false" by literal substring because
        // 'false' could appear inside a logical_name; the regex
        // would be brittle. Spot-check the column instead by
        // counting "    false    " patterns.)
        assert!(
            !out.contains(" false "),
            "stub predicate should not flag any node as flattened:\n{out}"
        );
    }

    /// Multi-phase workload — verify each phase appears with its
    /// own logical_name path, and the bind_outer column points
    /// at the materialised ancestor (here itself, since stub
    /// materialises everything).
    #[test]
    fn multi_phase_workload_lists_every_phase() {
        let yaml = r#"
phases:
  setup:
    ops:
      - op: noop
  run:
    ops:
      - op: noop
"#;
        let out =
            render_elision_summary(yaml, "two.yaml").expect("two-phase workload should render");
        assert!(
            out.contains("phase.setup"),
            "setup phase row missing:\n{out}"
        );
        assert!(out.contains("phase.run"), "run phase row missing:\n{out}");
    }

    /// Bad workload YAML surfaces an Err with the file path
    /// embedded, so the diagnostic tells the user *which* file
    /// failed (matters when running the binary against a
    /// directory of workloads or via shell expansion).
    #[test]
    fn malformed_workload_returns_path_tagged_error() {
        let bad = "not: valid: yaml: workload";
        let err = render_elision_summary(bad, "bad.yaml").unwrap_err();
        assert!(
            err.contains("bad.yaml"),
            "error should embed the offending path: {err}"
        );
    }

    /// `bind_outer` for a materialised node points at its own
    /// logical_name (so the summary is self-describing). When
    /// SRD-13d Phase 3 ships and a non-trivial predicate flags
    /// some node as flattened, the same row will instead point
    /// at the nearest materialised ancestor — but the column
    /// shape stays.
    #[test]
    fn bind_outer_column_is_self_when_node_is_materialised() {
        let yaml = r#"
phases:
  p:
    ops:
      - op: noop
"#;
        let out = render_elision_summary(yaml, "x.yaml").unwrap();
        // The phase row should mention its own name twice — once
        // in the logical_name column, once in bind_outer.
        let phase_lines: Vec<&str> = out.lines().filter(|l| l.contains("phase.p")).collect();
        assert_eq!(
            phase_lines.len(),
            1,
            "expected exactly one phase row, got: {phase_lines:?}"
        );
        let line = phase_lines[0];
        let occurrences = line.matches("workload.scenario.default.phase.p").count();
        assert_eq!(
            occurrences, 2,
            "materialised phase row should list its logical_name twice (logical_name + bind_outer): {line}"
        );
    }
}

#[cfg(test)]
mod describe_wrappers_tests {
    //! SRD-32a Push 4 — discoverability commands. Tests pin the
    //! shape of `nmbrs describe wrappers` and `nmbrs describe op`
    //! so the human-readable surface doesn't drift silently.
    use super::{render_op_description, render_wrappers_table};

    /// The wrapper table must include every built-in wrapper
    /// from the registry. The registry is alphabetical, so the
    /// rows arrive in alphabetical order by wrapper name.
    #[test]
    fn wrappers_table_lists_every_built_in() {
        let out = render_wrappers_table();
        // Header row.
        assert!(out.contains("NAME"), "header missing NAME column:\n{out}");
        assert!(
            out.contains("OWNED FIELDS"),
            "header missing OWNED FIELDS:\n{out}"
        );
        assert!(out.contains("TRIGGER"), "header missing TRIGGER:\n{out}");
        assert!(
            out.contains("CONSTRAINTS"),
            "header missing CONSTRAINTS:\n{out}"
        );
        // Each registered wrapper appears.
        for name in [
            "traverse", "delay", "validate", "poll", "if", "fields", "result", "metrics",
        ] {
            assert!(
                out.contains(name),
                "wrapper `{name}` missing from describe wrappers output:\n{out}"
            );
        }
    }

    /// Trigger labels for the built-in wrappers — matches the
    /// SRD §"Discoverability" prose. Pinning these strings keeps
    /// the documentation surface stable.
    #[test]
    fn wrappers_table_uses_human_trigger_labels() {
        let out = render_wrappers_table();
        // The traverse row must say "always".
        let traverse_line = out
            .lines()
            .find(|l| l.starts_with("traverse"))
            .expect("traverse row missing");
        assert!(
            traverse_line.contains("always"),
            "traverse trigger should be `always`: {traverse_line}"
        );
        let validate_line = out
            .lines()
            .find(|l| l.starts_with("validate"))
            .expect("validate row missing");
        assert!(
            validate_line.contains("verify/relevancy"),
            "validate trigger should mention verify/relevancy: {validate_line}"
        );
        let metrics_line = out
            .lines()
            .find(|l| l.starts_with("metrics"))
            .expect("metrics row missing");
        assert!(
            metrics_line.contains("non-empty metrics map"),
            "metrics trigger should be `non-empty metrics map`: {metrics_line}"
        );
        // metrics declares forbids_outer for every other wrapper —
        // surface that in the constraints column.
        assert!(
            metrics_line.contains("forbids_outer="),
            "metrics row should advertise its forbids_outer constraint: {metrics_line}"
        );
    }

    /// Owned-fields column lists the registry's owned-field names
    /// (validate, poll, etc.). Wrappers with no owned fields must
    /// render `(none)` rather than an empty cell.
    #[test]
    fn wrappers_table_owned_fields_column_uses_none_for_empty() {
        let out = render_wrappers_table();
        let traverse_line = out
            .lines()
            .find(|l| l.starts_with("traverse"))
            .expect("traverse row missing");
        assert!(
            traverse_line.contains("(none)"),
            "traverse owned fields should render as `(none)`: {traverse_line}"
        );
        let validate_line = out
            .lines()
            .find(|l| l.starts_with("validate"))
            .expect("validate row missing");
        for f in ["verify", "relevancy", "strict"] {
            assert!(
                validate_line.contains(f),
                "validate row missing owned field `{f}`: {validate_line}"
            );
        }
    }

    /// `describe op` against a workload that defines a phase op
    /// renders the resolved stack innermost-to-outermost, names
    /// the phase, and labels each line. The empty `noop` op only
    /// triggers `traverse` + `result`, so the stack is two lines.
    #[test]
    fn describe_op_simple_phase_shows_default_stack() {
        let yaml = r#"
phases:
  setup:
    ops:
      noop:
        stmt: "noop"
"#;
        let dir = std::env::temp_dir().join("nmbrs_describe_op_simple");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("simple.yaml");
        std::fs::write(&path, yaml).expect("write workload");

        let out = render_op_description(path.to_str().unwrap(), "noop")
            .expect("simple workload should resolve");
        assert!(out.contains("op 'noop'"), "header missing op name: {out}");
        assert!(out.contains("phase: setup"), "header missing phase: {out}");
        assert!(
            out.contains("wrapper stack (innermost -> outermost)"),
            "stack header missing: {out}"
        );
        // Empty op fires only the always-on wrappers.
        let traverse_idx = out.find("traverse").expect("traverse missing");
        let result_idx = out.find("result").expect("result missing");
        assert!(
            traverse_idx < result_idx,
            "traverse should print before result: {out}"
        );
        // None of the optional wrappers should appear in the
        // stack. We check the numbered stack lines so a phase or
        // op whose name contains "metrics" or "validate" can't
        // false-positive a substring search.
        let stack_lines: Vec<&str> = out
            .lines()
            .filter(|l| l.trim_start().starts_with(|c: char| c.is_ascii_digit()))
            .collect();
        for unexpected in ["delay", "validate", "poll", "fields", "metrics"] {
            for line in &stack_lines {
                assert!(
                    !line.contains(unexpected),
                    "unexpected wrapper `{unexpected}` in stack line: {line}"
                );
            }
        }
    }

    /// An op declaring `verify:` triggers validate; the resolver
    /// pulls in traverse transitively. Provenance text must
    /// distinguish the two activations.
    #[test]
    fn describe_op_validate_shows_owned_field_and_transitive() {
        let yaml = r#"
phases:
  go:
    ops:
      check:
        stmt: "SELECT 1"
        verify: "min_rows >= 1"
"#;
        let dir = std::env::temp_dir().join("nmbrs_describe_op_validate");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("validate.yaml");
        std::fs::write(&path, yaml).expect("write workload");

        let out = render_op_description(path.to_str().unwrap(), "check")
            .expect("validate workload should resolve");
        // Validate fires on `verify:` — the line should say so.
        assert!(
            out.contains("triggered by `verify:` field"),
            "validate provenance missing: {out}"
        );
        // Traverse is transitive (always-on wrapper, but it would
        // also be pulled in transitively by validate). The
        // resolver tags it AlwaysOn because the trigger fires
        // first. Either way, the line should not falsely claim
        // a `verify:` trigger.
        let traverse_line = out
            .lines()
            .find(|l| l.contains("1.") && l.contains("traverse"))
            .expect("traverse line missing");
        assert!(
            !traverse_line.contains("triggered by"),
            "traverse line should not claim a field trigger: {traverse_line}"
        );
    }

    /// Unknown op-template names surface a clean error including
    /// the candidate list. The error path must NOT panic and
    /// must not propagate a Debug-formatted ResolveError.
    #[test]
    fn describe_op_unknown_lists_candidates() {
        let yaml = r#"
phases:
  go:
    ops:
      alpha:
        stmt: "noop"
      beta:
        stmt: "noop"
"#;
        let dir = std::env::temp_dir().join("nmbrs_describe_op_unknown");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("two.yaml");
        std::fs::write(&path, yaml).expect("write workload");

        let err = render_op_description(path.to_str().unwrap(), "missing").unwrap_err();
        assert!(
            err.contains("no op template named 'missing'"),
            "error should name the missing template: {err}"
        );
        assert!(
            err.contains("alpha"),
            "candidate list should include alpha: {err}"
        );
        assert!(
            err.contains("beta"),
            "candidate list should include beta: {err}"
        );
    }

    /// Missing-file path returns a clean error string, not a
    /// panic and not a Debug-format. The path must be embedded so
    /// the operator sees what was attempted.
    #[test]
    fn describe_op_missing_file_returns_clean_error() {
        let err = render_op_description("/nonexistent/path/never.yaml", "x").unwrap_err();
        assert!(
            err.contains("never.yaml"),
            "error should embed the file path: {err}"
        );
    }
}
