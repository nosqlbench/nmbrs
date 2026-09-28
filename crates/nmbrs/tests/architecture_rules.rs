// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! CI gate for the crate/module dependency rules —
//! `docs/SRD/05_dependency_rules.md`. This is Pillar 5 (Enforced
//! edges) of the Subsystem Treatment Standard
//! (`docs/SRD/00b_subsystem_standard.md`) made machine-checkable.
//!
//! - **D2** — no upward edges: every `[dependencies]` edge points to
//!   a strictly lower layer (subsumes "foundation crates never depend
//!   on the integration / presentation tier", i.e. D3).
//! - **D4** — adapters don't depend on adapters, except the
//!   allowlisted `testkit → stdout`.
//! - **D6** — no workspace crate reaches past polydat's public
//!   surface into a deep internal path.
//! - **D5** — consumers honor each crate's declared public surface.
//!   Staged `#[ignore]` until `nmbrs-runtime`'s ~50-module surface is
//!   narrowed to its declared Contract Registry row.
//!
//! The layer map below is the *specification*: the test asserts the
//! live `Cargo.toml` graph matches it. Adding a workspace crate
//! without giving it a layer fails `d0_every_crate_has_a_layer`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Canonical layer assignment (see SRD-05 §Dependency layers).
/// `[dependencies]` edges may only point to a strictly lower layer.
fn layer(crate_name: &str) -> Option<u32> {
    Some(match crate_name {
        // L0 — leaf substrates (zero internal deps). cassandra-cpp is
        // a vendored fork; it has no internal deps, so L0 keeps the
        // cql→cassandra edge valid without special-casing.
        "nmbrs-errorhandler" | "cassandra-cpp" => 0,
        // SRD-86 §"The metric-reader surface" — metricsql is the query
        // language ATOP nmbrs-metrics' data-access library: it evaluates
        // over the metrics `queryapi` (Vector shape + MetricAccess), so
        // it sits above metrics (L1). No longer a
        // standalone L0 leaf, by design.
        "nmbrs-metricsql" => 2,
        // SRD-86 — the optimizer algorithms are an inventory PLUGIN: they
        // register against the core contract (defined in nmbrs-runtime) and
        // are discovered via inventory, so the crate depends on the core and
        // sits ABOVE it, exactly like an adapter. The core never names it.
        "nmbrs-optimizers" => 4,
        "nmbrs-metrics" | "nmbrs-workload" => 1,
        "nmbrs-rate" | "nmbrs-adapter-openapi" => 2,
        "nmbrs-runtime" => 3,
        "nmbrs-adapter-stdout"
        | "nmbrs-adapter-http"
        | "nmbrs-adapter-plotter"
        | "nmbrs-adapter-cql"
        | "nmbrs-tui"
        | "nmbrs-web" => 4,
        "nmbrs-adapter-testkit" => 5,
        "nmbrs" => 6,
        _ => return None,
    })
}

const ADAPTERS: &[&str] = &[
    "nmbrs-adapter-stdout",
    "nmbrs-adapter-http",
    "nmbrs-adapter-plotter",
    "nmbrs-adapter-cql",
    "nmbrs-adapter-openapi",
    "nmbrs-adapter-testkit",
];

/// The single allowlisted adapter→adapter edge (D4).
const ADAPTER_EDGE_ALLOW: &[(&str, &str)] = &[("nmbrs-adapter-testkit", "nmbrs-adapter-stdout")];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("nmbrs crate lives in crates/")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// Member directories: the root manifest declares `members = ["crates/*"]`,
/// so every directory under `crates/` holding a `Cargo.toml` is a member.
fn workspace_members(root: &Path) -> Vec<PathBuf> {
    let txt = fs::read_to_string(root.join("Cargo.toml")).expect("read root Cargo.toml");
    assert!(
        txt.contains(r#"members = ["crates/*"]"#),
        "root Cargo.toml no longer declares `members = [\"crates/*\"]`; update workspace_members()"
    );
    let mut out: Vec<PathBuf> = fs::read_dir(root.join("crates"))
        .expect("read crates/")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("Cargo.toml").is_file())
        .collect();
    out.sort();
    out
}

struct Manifest {
    name: String,
    deps: BTreeSet<String>,
    dev_deps: BTreeSet<String>,
}

/// Minimal, dependency-free Cargo.toml parse: package name + the
/// dependency *keys* in `[dependencies]` / `[dev-dependencies]`
/// (and their dotted-table forms). Internal edges are recovered by
/// intersecting keys with the set of workspace package names.
fn parse_manifest(dir: &Path) -> Manifest {
    let txt = fs::read_to_string(dir.join("Cargo.toml"))
        .unwrap_or_else(|e| panic!("read {}/Cargo.toml: {e}", dir.display()));
    let mut name = String::new();
    let mut deps = BTreeSet::new();
    let mut dev_deps = BTreeSet::new();
    #[derive(PartialEq)]
    enum Sec {
        Package,
        Deps,
        DevDeps,
        Other,
    }
    let mut sec = Sec::Other;

    for raw in txt.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            // Section header. Handle dotted sub-tables that name a dep
            // directly, e.g. `[dependencies.foo]`.
            let header = line.trim_start_matches('[').trim_end_matches(']');
            if header == "package" {
                sec = Sec::Package;
            } else if header == "dependencies" || header.ends_with(".dependencies") {
                sec = Sec::Deps;
            } else if header == "dev-dependencies" || header.ends_with(".dev-dependencies") {
                sec = Sec::DevDeps;
            } else if let Some(rest) = header.strip_prefix("dependencies.") {
                deps.insert(rest.to_string());
                sec = Sec::Other;
            } else if let Some(rest) = header.strip_prefix("dev-dependencies.") {
                dev_deps.insert(rest.to_string());
                sec = Sec::Other;
            } else {
                sec = Sec::Other;
            }
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match sec {
            Sec::Package => {
                if let Some(v) = line.strip_prefix("name")
                    && let Some(q) = v.find('"')
                    && let Some(end) = v[q + 1..].find('"')
                {
                    name = v[q + 1..q + 1 + end].to_string();
                }
            }
            Sec::Deps | Sec::DevDeps => {
                // dep key is the token before `=`, then before any `.`
                if let Some(eq) = line.find('=') {
                    let key = line[..eq].trim();
                    let key = key.split('.').next().unwrap_or(key).trim();
                    let key = key.trim_matches('"');
                    if !key.is_empty() {
                        if sec == Sec::Deps {
                            deps.insert(key.to_string());
                        } else {
                            dev_deps.insert(key.to_string());
                        }
                    }
                }
            }
            Sec::Other => {}
        }
    }
    assert!(!name.is_empty(), "no package name in {}", dir.display());
    Manifest {
        name,
        deps,
        dev_deps,
    }
}

/// Build the internal dependency graph (edges restricted to workspace crates).
fn load_graph() -> BTreeMap<String, Manifest> {
    let root = workspace_root();
    let manifests: Vec<Manifest> = workspace_members(&root)
        .iter()
        .map(|d| parse_manifest(d))
        .collect();
    let names: BTreeSet<String> = manifests.iter().map(|m| m.name.clone()).collect();
    manifests
        .into_iter()
        .map(|mut m| {
            m.deps.retain(|d| names.contains(d));
            m.dev_deps.retain(|d| names.contains(d));
            (m.name.clone(), m)
        })
        .collect()
}

#[test]
fn d0_every_crate_has_a_layer() {
    let g = load_graph();
    let missing: Vec<&String> = g.keys().filter(|n| layer(n).is_none()).collect();
    assert!(
        missing.is_empty(),
        "workspace crates without a layer in SRD-05 (add them to `layer()` + the SRD): {missing:?}"
    );
}

#[test]
fn d2_edges_point_down() {
    let g = load_graph();
    let mut violations = Vec::new();
    for (name, m) in &g {
        let Some(ln) = layer(name) else { continue };
        for dep in &m.deps {
            let Some(ld) = layer(dep) else { continue };
            if ld >= ln {
                violations.push(format!("{name} (L{ln}) -> {dep} (L{ld})"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "D2: [dependencies] edges must point to a strictly lower layer. Upward/level edges:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn d4_no_cross_adapter_edges() {
    let g = load_graph();
    let adapters: BTreeSet<&str> = ADAPTERS.iter().copied().collect();
    let allow: BTreeSet<(&str, &str)> = ADAPTER_EDGE_ALLOW.iter().copied().collect();
    let mut violations = Vec::new();
    for name in ADAPTERS {
        let Some(m) = g.get(*name) else { continue };
        for dep in &m.deps {
            if adapters.contains(dep.as_str()) && !allow.contains(&(*name, dep.as_str())) {
                violations.push(format!("{name} -> {dep}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "D4: adapters must not depend on other adapters (allowlist: testkit->stdout). Found:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn d6_no_polydat_deep_paths() {
    // Forbidden deep-internal polydat paths that bypass the public surface.
    const FORBIDDEN: &[&str] = &["polydat::compile::jit", "polydat::library::support"];
    let root = workspace_root();
    let mut hits = Vec::new();
    for member in workspace_members(&root) {
        let src = member.join("src");
        if src.is_dir() {
            scan_rs(&src, FORBIDDEN, &mut hits);
        }
    }
    assert!(
        hits.is_empty(),
        "D6: non-polydat crates must not use deep polydat internals:\n  {}",
        hits.join("\n  ")
    );
}

fn scan_rs(dir: &Path, needles: &[&str], hits: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            scan_rs(&p, needles, hits);
        } else if p.extension().and_then(|s| s.to_str()) == Some("rs")
            && let Ok(txt) = fs::read_to_string(&p)
        {
            for (i, line) in txt.lines().enumerate() {
                for n in needles {
                    if line.contains(n) {
                        hits.push(format!("{}:{}: {}", p.display(), i + 1, line.trim()));
                    }
                }
            }
        }
    }
}

/// D5 — each crate's declared-internal modules stay `pub(crate)`, never
/// bare `pub`. The **compiler** is the real enforcement: a `pub(crate)`
/// module is unreachable from any foreign crate, so the sprawl can't be
/// re-consumed. This test is the regression guard that keeps those
/// modules narrowed — re-widening one back to `pub` (re-growing the
/// surface) fails here. Modules used by the crate's OWN integration
/// tests (white-box tests are external crates) legitimately stay `pub`
/// and are NOT listed below. See SRD-05 §Contract Registry and the
/// front-door SRDs 08 / 29 / 59.
#[test]
fn d5_public_surface() {
    // (crate dir, modules that must be `pub(crate) mod`). Only
    // *workspace-internal* crates appear here: their public API is "what the
    // workspace consumes". Standalone libraries (nmbrs-metricsql, nmbrs-rate,
    // nmbrs-errorhandler) are exempt — their public
    // API is their own library contract, broader than any one consumer.
    let internal: &[(&str, &[&str])] = &[
        (
            "crates/nmbrs-tui",
            &[
                "widgets",
                "frame_broker",
                "prompt_state",
                "readout_panel",
                "readout_sink",
                "tui_sink",
            ],
        ),
        (
            "crates/nmbrs-runtime",
            &[
                "adapters",
                "params",
                "scope_elision",
                "phase_filter",
                "phase_params",
                "scheduler",
                "profiler",
                "trace_router",
                "executor",
                "error_policy",
                "stop_conditions",
                "workload_shell",
                "describe",
                "wrapper_registrations",
                "relevancy",
                "fiber_pool",
                "daemon_pool",
                "readout_context",
            ],
        ),
    ];
    let root = workspace_root();
    let mut violations = Vec::new();
    for (crate_dir, mods) in internal {
        let lib = root.join(crate_dir).join("src/lib.rs");
        let txt =
            fs::read_to_string(&lib).unwrap_or_else(|e| panic!("read {}: {e}", lib.display()));
        for m in *mods {
            let crated = format!("pub(crate) mod {m};");
            if !txt.contains(&crated) {
                let why = if txt.contains(&format!("pub mod {m};")) {
                    format!("`pub mod {m};` re-widened — must be `pub(crate) mod {m};`")
                } else {
                    format!("declared-internal module `{m}` not found as `pub(crate) mod {m};`")
                };
                violations.push(format!("{crate_dir}: {why}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "D5: declared-internal modules must stay `pub(crate)` (don't re-grow the surface):\n  {}",
        violations.join("\n  ")
    );
}

/// SRD-87 A1 — **no fd bypass**. The op-output / raster / readout *producer*
/// paths submit through `OutputChannel`; they must never write an fd
/// directly. This gate asserts those specific paths contain zero raw
/// terminal-write macros (`println!` / `eprintln!` / `print!` / `eprint!`),
/// locking in SRD-87 pushes 1–3. A new raw write in a producer path fails
/// here — route it through the right bucket
/// (`output_channel::{op_output,raster,status,log_to_surface}` /
/// `nmbrs_runtime::diag!`) instead.
///
/// Scope is deliberately the *producers that race the live display*, NOT
/// every `eprintln!` in the tree: the documented carve-outs — the channel
/// itself (`output_channel.rs`, which owns the fds), bootstrap / session
/// errors, the post-run summary (printed after the sink tears down), and CLI
/// subcommands (`describe` / `metrics` / `report` / `replay` / `openapi`,
/// one-shot tools) — are out of scope per SRD-87 §6/§11.
#[test]
fn a1_output_channel_no_fd_bypass() {
    // Raw terminal-write macros. `write!`/`writeln!` are excluded: they target
    // an explicit writer (a file `OutputTarget`, a `String` buffer), not an
    // implicit fd. Bare `io::stdout()` is excluded too — the producers use it
    // for TTY detection and for constructing the file-mode `BufWriter` target,
    // neither of which is a console write.
    const FORBIDDEN: &[&str] = &["println!", "eprintln!", "print!", "eprint!"];
    let root = workspace_root();
    // The producer paths (files + dirs) relative to the workspace root.
    let producers: &[&str] = &[
        "crates/nmbrs-adapter-stdout/src/lib.rs",
        "crates/nmbrs-adapter-plotter/src/lib.rs",
        "crates/nmbrs-adapter-testkit/src/lib.rs",
        "crates/nmbrs-runtime/src/readouts",
    ];
    let mut hits = Vec::new();
    for rel in producers {
        let path = root.join(rel);
        if path.is_dir() {
            scan_no_fd_writes_dir(&path, FORBIDDEN, &mut hits);
        } else {
            scan_no_fd_writes_file(&path, FORBIDDEN, &mut hits);
        }
    }
    assert!(
        hits.is_empty(),
        "SRD-87 A1: op-output/raster/readout producers must not write an fd \
         directly — route through the OutputChannel bucket instead:\n  {}",
        hits.join("\n  ")
    );
}

fn scan_no_fd_writes_dir(dir: &Path, needles: &[&str], hits: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            scan_no_fd_writes_dir(&p, needles, hits);
        } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
            scan_no_fd_writes_file(&p, needles, hits);
        }
    }
}

/// Scan one `.rs` file for `needles`, skipping comment text and the trailing
/// `#[cfg(test)]` test module (tests legitimately print for assertions).
fn scan_no_fd_writes_file(path: &Path, needles: &[&str], hits: &mut Vec<String>) {
    let Ok(txt) = fs::read_to_string(path) else {
        return;
    };
    for (i, raw) in txt.lines().enumerate() {
        // A test module (conventionally at the file tail) ends production
        // code: stop scanning the file at its start.
        let trimmed = raw.trim_start();
        if trimmed.starts_with("#[cfg(test)]") || trimmed == "mod tests {" {
            break;
        }
        // Strip line/doc comments so a macro name *mentioned* in a comment
        // isn't a hit; only real code counts.
        let code = match raw.find("//") {
            Some(idx) => &raw[..idx],
            None => raw,
        };
        for n in needles {
            if code.contains(n) {
                hits.push(format!("{}:{}: {}", path.display(), i + 1, raw.trim()));
            }
        }
    }
}
