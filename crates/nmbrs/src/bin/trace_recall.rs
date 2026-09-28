// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `trace-recall` — group/aggregate `event=relevancy.score`
//! trace events emitted by the production validation pipeline.
//!
//! Reads one or more `--trace=` files (produced by the
//! `trace_router` infrastructure) and reports:
//!
//! - Overall n + mean of all `event=relevancy.score` events.
//! - Per-predicate breakdown (n, mean) when a `--dataset-root`
//!   is supplied: the cycle ordinal in each event is looked
//!   up in `<root>/profiles/base/predicates.u8` to determine
//!   which metadata predicate the query carried, so we can
//!   slice a default-profile sweep (10000 cycles, mixed
//!   predicates) down to "just the predicate=N queries"
//!   apples-to-apples against a per-label profile run.
//!
//! Trace event grammar (per [`nmbrs_runtime::observer::trace`]
//! at the relevancy scoring site):
//!
//! ```text
//!   <ts> TRC event=relevancy.score cycle=<N> fn=<name> k=<K> r=<R>
//!        gt_card=<G> actual_card=<A> intersect=<I> score=<F>
//! ```
//!
//! Usage:
//!
//! ```text
//! trace-recall --trace-file <path> [--trace-file <path>] \
//!              [--dataset-root <path>]
//! ```

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use vectordata::{
    TestDataGroup, catalog::resolver::Catalog, catalog::sources::CatalogSources, open_facet_typed,
};

fn main() -> ExitCode {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    let predicates: Option<Vec<u8>> = match cfg.dataset.as_deref() {
        Some(spec) => match load_predicates_via_catalog(spec) {
            Ok(v) => {
                println!(
                    "predicates: {} u8 values via catalog dataset '{spec}'",
                    v.len()
                );
                Some(v)
            }
            Err(e) => {
                eprintln!("warning: load predicates via catalog: {e} (no per-predicate slicing)");
                None
            }
        },
        None => None,
    };

    for path in &cfg.trace_files {
        match analyse(path, predicates.as_deref()) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("{}: {e}", path.display());
            }
        }
    }
    ExitCode::SUCCESS
}

struct Config {
    trace_files: Vec<PathBuf>,
    /// Catalog dataset spec (name, path, or URL) used to load the
    /// `metadata_predicates` facet through the vectordata API —
    /// the SAME source the PVS workload uses at runtime, so the
    /// per-predicate slicing here is guaranteed to match the
    /// production run's predicate assignments.
    dataset: Option<String>,
}

fn parse_args() -> Result<Config, String> {
    let args: Vec<String> = std::env::args().collect();
    let mut trace_files = Vec::new();
    let mut dataset: Option<String> = None;
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--trace-file" => {
                trace_files.push(PathBuf::from(
                    it.next().ok_or("--trace-file requires a path")?,
                ));
            }
            "--dataset" => {
                dataset = Some(it.next().ok_or("--dataset requires a value")?.clone());
            }
            "--help" | "-h" => {
                eprintln!(
                    "usage: trace-recall --trace-file <path> [--trace-file <path>] \
                     [--dataset <name|path|url>]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown arg: {other}")),
        }
    }
    if trace_files.is_empty() {
        return Err("at least one --trace-file required".into());
    }
    Ok(Config {
        trace_files,
        dataset,
    })
}

/// Load `metadata_predicates` as a `Vec<u8>` through the
/// vectordata catalog API. Uses the SAME resolver path the
/// production workload uses, so we read identical bytes to
/// what the PVS run consumed for `predicate_value_at(...)`.
fn load_predicates_via_catalog(spec: &str) -> Result<Vec<u8>, String> {
    let catalog = Catalog::of(&CatalogSources::new().configure_default());
    let group: TestDataGroup = catalog
        .open(spec)
        .map_err(|e| format!("catalog open '{spec}': {e}"))?;
    // Try each profile until one exposes `metadata_predicates`
    // (the catalog projects shared base facets onto profiles).
    for p in group.profile_names() {
        let Some(view) = group.generic_view(&p) else {
            continue;
        };
        if let Ok(reader) = open_facet_typed::<u8>(&view, "metadata_predicates") {
            let n = reader.count();
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                out.push(reader.get_native(i));
            }
            return Ok(out);
        }
    }
    Err("no profile exposes `metadata_predicates`".into())
}

fn analyse(path: &Path, predicates: Option<&[u8]>) -> Result<(), String> {
    let f = File::open(path).map_err(|e| format!("open: {e}"))?;
    let reader = BufReader::new(f);

    let mut overall_n: u64 = 0;
    let mut overall_sum: f64 = 0.0;
    let mut overall_min: f64 = f64::INFINITY;
    let mut overall_max: f64 = f64::NEG_INFINITY;
    // predicate-byte -> (n, sum)
    let mut by_pred: BTreeMap<u8, (u64, f64)> = BTreeMap::new();

    for line in reader.lines() {
        let line = line.map_err(|e| format!("read: {e}"))?;
        if !line.contains("event=relevancy.score") {
            continue;
        }
        let cycle = match extract_field(&line, "cycle=") {
            Some(s) => s.parse::<u64>().map_err(|e| format!("cycle parse: {e}"))?,
            None => continue,
        };
        let score = match extract_field(&line, "score=") {
            Some(s) => s.parse::<f64>().map_err(|e| format!("score parse: {e}"))?,
            None => continue,
        };
        overall_n += 1;
        overall_sum += score;
        overall_min = overall_min.min(score);
        overall_max = overall_max.max(score);
        if let Some(pred) = predicates
            && let Some(&p) = pred.get(cycle as usize)
        {
            let e = by_pred.entry(p).or_insert((0, 0.0));
            e.0 += 1;
            e.1 += score;
        }
    }

    println!("\n=== {} ===", path.display());
    if overall_n == 0 {
        println!("  no event=relevancy.score lines found");
        return Ok(());
    }
    println!(
        "  overall: n={overall_n} mean={:.6} min={:.6} max={:.6}",
        overall_sum / overall_n as f64,
        overall_min,
        overall_max
    );
    if !by_pred.is_empty() {
        println!("  by predicate (from predicates.u8 lookup on cycle ordinal):");
        for (p, (n, s)) in &by_pred {
            println!(
                "    predicate={:>2}  n={:>5}  mean={:.6}",
                p,
                n,
                s / *n as f64
            );
        }
    }
    Ok(())
}

fn extract_field(line: &str, key: &str) -> Option<String> {
    let pos = line.find(key)?;
    let after = &line[pos + key.len()..];
    let end = after
        .find(|c: char| c.is_whitespace())
        .unwrap_or(after.len());
    Some(after[..end].to_string())
}
