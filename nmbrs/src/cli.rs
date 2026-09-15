// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Usage text and utility functions shared across subcommands.
//!
//! Shell completion is owned by [`nmbrs_runtime::completions`] — the
//! same harness `nmbrs` uses — so `nmbrs run workload=<TAB>`,
//! `scenario=<TAB>`, `adapter=<TAB>`, etc. all expand identically
//! regardless of which adapter features are linked. `main.rs` wires
//! it up; nothing in this file duplicates that logic.

/// Legacy human-readable usage text. Today's CLI surface is built
/// from `cli_spec` and renders help via that path; this function
/// is retained as a fallback writer for tooling that wants the
/// classic flat block.
#[allow(dead_code)]
pub fn print_usage() {
    eprintln!("nmbrs — nosqlbench for Rust");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  nmbrs run adapter=stdout workload=file.yaml cycles=100 threads=4");
    eprintln!("  nmbrs run workload=file.yaml tags=block:main rate=1000 format=json");
    eprintln!("  nmbrs run op='hello {{{{cycle}}}}' cycles=10");
    eprintln!("  nmbrs run op='id={{{{mod(hash(cycle), 1000)}}}}' cycles=100 format=json");
    eprintln!("  nmbrs attach                  Attach to a running nmbrs over its OOB socket");
    eprintln!("  nmbrs attach --pid <N>        Attach to a specific running instance");
    eprintln!("  nmbrs attach -c phases        One-shot: run a command and exit");
    eprintln!("  nmbrs summary                 List stored summaries in logs/latest/metrics.db");
    eprintln!("  nmbrs summary all             Render every stored named summary");
    eprintln!("  nmbrs summary --name <NAME>   Render the stored summary <NAME>");
    eprintln!("  nmbrs summary '*'             Ad-hoc all-metrics report");
    eprintln!("  nmbrs summary --name <NAME> --create '<spec>'  Persist + render");
    eprintln!(
        "  nmbrs describe wiring functions [-v]  List wiring functions (verbose: + signatures, types, associativity)"
    );
    eprintln!("  nmbrs describe wiring functions-md    Dump all functions to markdown file");
    eprintln!("  nmbrs describe wiring stdlib          List standard library modules");
    eprintln!(
        "  nmbrs describe wiring dag <file>      Render a wiring source file as DOT/Mermaid/SVG"
    );
    eprintln!(
        "  nmbrs bench wiring <expr>    Benchmark a wiring expression at all compilation levels"
    );
    eprintln!(
        "  nmbrs wiring visualize <expr> Evaluate a wiring expression and plot outputs to terminal"
    );
    eprintln!("  nmbrs wiring visualize <file> Plot a wiring file's outputs to the terminal");
    eprintln!(
        "  nmbrs web [bind=127.0.0.1] [port=8080]  Start the web dashboard (bind=0.0.0.0 to expose)"
    );
    eprintln!("  nmbrs web --daemon             Start web dashboard in the background");
    eprintln!("  nmbrs web --stop               Stop a running background web dashboard");
    eprintln!("  nmbrs web --restart            Restart with the same arguments");
    eprintln!();
    eprintln!("Parameters:");
    eprintln!("  workload=<file.yaml>   Workload definition file");
    eprintln!("  adapter=<name>         Adapter type (default: stdout)");
    eprintln!("  cycles=<n>             Number of cycles to execute");
    eprintln!("  threads=<n>            Concurrency level (default: 1)");
    eprintln!("  rate=<n>               Rate limit (ops/sec)");
    eprintln!("  tags=<filter>          Tag filter for op selection");
    eprintln!("  seq=<type>             Sequencer: bucket|interval|concat");
    eprintln!("  format=<type>          Output format: assignments|json|csv|stmt");
    eprintln!("  errors=<spec>          Error handler spec");
    eprintln!("  filename=<path>        Output file (default: stdout)");
    eprintln!("  --report-openmetrics-to=<url>  Push metrics in OpenMetrics format");
    eprintln!("                         e.g. http://localhost:8080/api/v1/import/prometheus");
}

/// Resolve a potential workload path, trying extensions if needed.
///
/// Returns `Some(path)` if a workload file exists, `None` otherwise.
pub fn resolve_workload_path(name: &str) -> Option<String> {
    if name.ends_with(".yaml") || name.ends_with(".yml") {
        if std::path::Path::new(name).exists() {
            return Some(name.to_string());
        }
        return None;
    }

    for ext in &[".yaml", ".yml"] {
        let path = format!("{name}{ext}");
        if std::path::Path::new(&path).exists() {
            return Some(path);
        }
    }

    for ext in &["", ".yaml", ".yml"] {
        let path = format!("workloads/{name}{ext}");
        if std::path::Path::new(&path).exists() {
            return Some(path);
        }
    }

    // Adapter-bundled workloads — the canonical home for
    // workloads shipped with each adapter crate. Probed last
    // so an explicit `workloads/` override always wins.
    // Pattern: `adapters/<adapter>/workloads/<name>{,.yaml,.yml}`.
    if let Ok(adapters_dir) = std::fs::read_dir("adapters") {
        for entry in adapters_dir.flatten() {
            for ext in &["", ".yaml", ".yml"] {
                let path = entry.path().join("workloads").join(format!("{name}{ext}"));
                if path.exists() {
                    return path.to_str().map(String::from);
                }
            }
        }
    }
    // Examples are always probed too — handy for ad-hoc
    // explorations where the user just types the example name
    // (e.g. `nmbrs plot --name X workload=feature_showcase`).
    for ext in &["", ".yaml", ".yml"] {
        let path = format!("examples/workloads/{name}{ext}");
        if std::path::Path::new(&path).exists() {
            return Some(path);
        }
    }

    None
}

/// Parse a bind address flexibly: bare IP, host:port, or full URL.
pub fn parse_bind_address(raw: &str, port_override: Option<&str>) -> (String, u16) {
    let default_port = 8080u16;

    let without_scheme = raw
        .strip_prefix("http://")
        .or_else(|| raw.strip_prefix("https://"))
        .unwrap_or(raw);

    let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);

    let (host, embedded_port) = if let Some(colon_pos) = host_port.rfind(':') {
        let maybe_port = &host_port[colon_pos + 1..];
        if let Ok(p) = maybe_port.parse::<u16>() {
            (host_port[..colon_pos].to_string(), Some(p))
        } else {
            (host_port.to_string(), None)
        }
    } else {
        (host_port.to_string(), None)
    };

    let port = port_override
        .and_then(|s| s.parse::<u16>().ok())
        .or(embedded_port)
        .unwrap_or(default_port);

    let host = if host.is_empty() {
        "127.0.0.1".to_string()
    } else {
        host
    };
    (host, port)
}

#[allow(dead_code)]
pub fn format_duration(d: std::time::Duration) -> String {
    let ns = d.as_nanos();
    if ns < 1_000 {
        format!("{ns} ns")
    } else if ns < 1_000_000 {
        format!("{:.1} us", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.2} ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.2} s", ns as f64 / 1_000_000_000.0)
    }
}
