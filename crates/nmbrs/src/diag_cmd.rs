// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `nmbrs diag` — offline diagnostics for workload troubleshooting.
//!
//! The arm for checks that answer "is it my system or is it my
//! inputs?" **without** touching a cluster. First resident:
//!
//! `nmbrs diag query-labels <dbdir>` — labeled vector dataset
//! self-consistency verification for filtered-kNN / oracle
//! recall work. When a vector workload reports bad recall, the
//! expensive instinct is to tune the database (index settling,
//! rerank, LIMIT sweeps); but recall is a comparison against
//! *stored ground truth*, which has its own failure modes the
//! database cannot cause: predicate facets disagreeing with row
//! metadata, partition-local → global ordinal translation
//! drift, label profiles rebuilt against different query
//! vectors. This check reads the dataset's facet files directly
//! and reports:
//!
//! 1. Query distribution across predicate labels.
//! 2. Per-label partition cardinality vs metadata cardinality
//!    (the ordinal-mapping contract).
//! 3. Whether every label profile's query vectors are
//!    byte-identical to the base profile's.
//! 4. Ground-truth overlap: oracle-partition neighbors
//!    (translated local → global) vs filtered ground truth.
//!
//! Reading the punchline table: overlap ≈ 1.000 means the
//! dataset is self-consistent — a recall deficit is real and
//! the system deserves the investigation. Overlap matching
//! your measured recall means the workload and database are
//! fine and the dataset artifacts are the bug.

use std::collections::{HashMap, HashSet};
use std::path::Path;

pub fn spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle_default(_p: ParsedCommand) -> Result<(), String> {
        eprintln!("nmbrs diag <check>");
        eprintln!("  query-labels <dbdir>   Labeled vector dataset self-consistency");
        eprintln!("                         (predicates vs metadata, ordinal mapping,");
        eprintln!("                         query identity, oracle-vs-filtered ground truth)");
        Ok(())
    }
    fn handle_query_labels(p: ParsedCommand) -> Result<(), String> {
        query_labels(&p.raw)
    }
    Command {
        name: "diag",
        help: "Offline diagnostics (`diag query-labels <dbdir>`): is it the system or the inputs?",
        category: Category::Tools,
        level: Level::FullSurface,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        subcommands: vec![Command {
            name: "query-labels",
            help: "Verify a labeled vector dataset's internal consistency (filtered-kNN ground truth).",
            category: Category::Tools,
            level: Level::FullSurface,
            flags: Vec::new(),
            kv_params: &[],
            dynamic_options: None,
            positionals: vec![crate::cli_spec::Positional {
                name: "dbdir",
                help: "Dataset directory containing profiles/.",
                kind: crate::cli_spec::PositionalKind::One,
                value: crate::cli_spec::ValueProvider::Custom(crate::completion::dirs_provider),
            }],
            subcommands: Vec::new(),
            handler: Some(Handler::Sync(handle_query_labels)),
            raw_args: true,
            completion_override: None,
        }],
        handler: Some(Handler::Sync(handle_default)),
        raw_args: false,
        completion_override: None,
    }
}

/// `nmbrs diag query-labels <dbdir>` — see the module docs.
fn query_labels(args: &[String]) -> Result<(), String> {
    let dataset_dir = args.iter().find(|a| !a.starts_with('-')).ok_or_else(|| {
        "usage: nmbrs diag query-labels <dbdir>\n\
             <dbdir> is the dataset directory containing `profiles/` \
             (base, default, label_NN partitions)."
            .to_string()
    })?;
    let base = Path::new(dataset_dir);
    if !base.is_dir() {
        return Err(format!("dataset directory not found: {}", base.display()));
    }
    eprintln!("Dataset: {}", base.display());

    // ── 1. Read predicates (query→label mapping) ─────────────────

    let predicates = read_u8_facet(&base.join("profiles/base/predicates.u8"))?;
    let num_queries = predicates.len();
    eprintln!("Predicates (query labels): {num_queries} entries");

    let mut label_query_indices: HashMap<u8, Vec<usize>> = HashMap::new();
    for (qi, &label) in predicates.iter().enumerate() {
        label_query_indices.entry(label).or_default().push(qi);
    }
    let mut sorted_labels: Vec<u8> = label_query_indices.keys().copied().collect();
    sorted_labels.sort();

    println!("\n## Query Distribution by Predicate Label\n");
    println!("| Label | Queries | Fraction |");
    println!("|-------|---------|----------|");
    for &label in &sorted_labels {
        let count = label_query_indices[&label].len();
        let frac = count as f64 / num_queries as f64;
        println!("| {label:>5} | {count:>7} | {frac:>8.4} |");
    }
    println!("| Total | {num_queries:>7} | {:.4} |", 1.0);

    // ── 2. Read metadata_content (base vector→label) ─────────────

    let metadata = read_u8_facet(&base.join("profiles/base/metadata_content.u8"))?;
    eprintln!("Metadata content (base labels): {} entries", metadata.len());

    let mut label_globals: HashMap<u8, Vec<usize>> = HashMap::new();
    for (global_ord, &label) in metadata.iter().enumerate() {
        label_globals.entry(label).or_default().push(global_ord);
    }

    println!("\n## Ordinal Mapping (metadata_content → partition)\n");
    println!("| Label | Global Vectors | Profile base_count | Match? |");
    println!("|-------|---------------|-------------------|--------|");
    for &label in &sorted_labels {
        let globals = label_globals.get(&label).map(|v| v.len()).unwrap_or(0);
        let profile_base =
            read_vec_count(&base.join(format!("profiles/label_{label:02}/base_vectors.fvec")))?;
        let matches = if globals == profile_base { "yes" } else { "NO" };
        println!("| {label:>5} | {globals:>13} | {profile_base:>17} | {matches:>6} |");
    }

    // ── 3. Check if label profiles share the same query vectors ──

    println!("\n## Query Vector Identity Check\n");
    let base_queries = read_raw(&base.join("profiles/base/query_vectors.fvec"))?;
    let base_query_count = fvec_count(&base_queries);
    let base_query_dim = fvec_dim(&base_queries);
    println!("Base query vectors: {base_query_count} × dim {base_query_dim}");

    for &label in &sorted_labels {
        let profile_queries =
            read_raw(&base.join(format!("profiles/label_{label:02}/query_vectors.fvec")))?;
        let pq_count = fvec_count(&profile_queries);
        let status = if base_queries == profile_queries {
            "identical"
        } else {
            "DIFFERENT"
        };
        println!("label_{label:02}: {pq_count} queries, {status} to base");
    }

    // ── 4. Ground truth comparison ───────────────────────────────
    //
    // For each query, compare:
    //   default/filtered_neighbor_indices[qi] (global ordinals)
    //   label_XX/neighbor_indices[qi] (local ordinals, translated to global)

    let default_fni = read_ivec(&base.join("profiles/default/filtered_neighbor_indices.ivec"))?;
    let fni_k = default_fni.first().map(|v| v.len()).unwrap_or(0);
    println!("\n## Ground Truth Comparison (max k={fni_k})\n");

    let mut profile_neighbors: HashMap<u8, Vec<Vec<i32>>> = HashMap::new();
    for &label in &sorted_labels {
        let ni = read_ivec(&base.join(format!("profiles/label_{label:02}/neighbor_indices.ivec")))?;
        profile_neighbors.insert(label, ni);
    }

    for k in [10, 100] {
        println!("### k={k}\n");
        println!("| Label | Queries | Overlap | Exact |");
        println!("|-------|---------|---------|-------|");

        let mut total_queries = 0usize;
        let mut total_overlap = 0.0f64;
        let mut total_exact = 0usize;

        for &label in &sorted_labels {
            let Some(globals) = label_globals.get(&label) else {
                continue;
            };
            let Some(pni) = profile_neighbors.get(&label) else {
                continue;
            };
            let query_indices = &label_query_indices[&label];

            let mut label_overlap = 0.0f64;
            let mut label_exact = 0usize;
            let mut label_count = 0usize;

            // The label profile carries one entry per base query.
            // For query qi, the oracle neighbors are pni[qi] in
            // partition-local ordinals; the filtered ground truth
            // is default_fni[qi] in global ordinals. Only queries
            // targeting this label have meaningful filtered GT.
            for &qi in query_indices {
                if qi >= default_fni.len() || qi >= pni.len() {
                    continue;
                }
                let filtered_global = &default_fni[qi];
                let oracle_local = &pni[qi];

                let ki = k.min(filtered_global.len()).min(oracle_local.len());
                if ki == 0 {
                    continue;
                }
                let oracle_global: Vec<i32> = oracle_local[..ki]
                    .iter()
                    .map(|&local| {
                        let l = local as usize;
                        if l < globals.len() {
                            globals[l] as i32
                        } else {
                            -1
                        }
                    })
                    .collect();

                let fg: HashSet<i32> = filtered_global[..ki].iter().copied().collect();
                let og: HashSet<i32> = oracle_global.iter().copied().collect();
                let overlap = fg.intersection(&og).count() as f64 / ki as f64;

                label_overlap += overlap;
                if filtered_global[..ki] == oracle_global[..] {
                    label_exact += 1;
                }
                label_count += 1;
            }

            let avg_overlap = if label_count > 0 {
                label_overlap / label_count as f64
            } else {
                0.0
            };
            let exact_pct = if label_count > 0 {
                label_exact as f64 / label_count as f64 * 100.0
            } else {
                0.0
            };
            println!("| {label:>5} | {label_count:>7} | {avg_overlap:>7.4} | {exact_pct:>4.1}% |");
            total_queries += label_count;
            total_overlap += label_overlap;
            total_exact += label_exact;
        }

        if total_queries > 0 {
            let avg = total_overlap / total_queries as f64;
            let exact_pct = total_exact as f64 / total_queries as f64 * 100.0;
            println!("| **All** | {total_queries:>7} | {avg:>7.4} | {exact_pct:>4.1}% |");
        }
        println!();
    }

    println!("Overlap = fraction of filtered GT (global) found in oracle GT (translated).");
    println!("1.000 = identical ground truth. <1.0 = the two GTs disagree.");
    Ok(())
}

// ── Facet readers (direct file access) ────────────────────────────

/// Read a `.u8` scalar facet file: raw bytes, one u8 per entry.
fn read_u8_facet(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))
}

/// Read a file as raw bytes (for identity comparison).
fn read_raw(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))
}

/// Read an ivec file: `[dim: i32 LE, data: i32×dim]` repeated.
fn read_ivec(path: &Path) -> Result<Vec<Vec<i32>>, String> {
    let data = read_raw(path)?;
    let mut result = Vec::new();
    let mut pos = 0;
    while pos + 4 <= data.len() {
        let dim = i32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        if pos + dim * 4 > data.len() {
            break;
        }
        let vec: Vec<i32> = (0..dim)
            .map(|i| i32::from_le_bytes(data[pos + i * 4..pos + i * 4 + 4].try_into().unwrap()))
            .collect();
        pos += dim * 4;
        result.push(vec);
    }
    Ok(result)
}

/// Count records in a count-prefixed vec file (fvec/ivec share
/// the `[dim, payload]` framing; record size = 4 + dim×4).
fn fvec_count(data: &[u8]) -> usize {
    if data.len() < 4 {
        return 0;
    }
    let dim = i32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    let record_size = 4 + dim * 4;
    if record_size == 4 {
        return 0;
    }
    data.len() / record_size
}

/// Dimension of the first record in a vec file.
fn fvec_dim(data: &[u8]) -> usize {
    if data.len() < 4 {
        return 0;
    }
    i32::from_le_bytes(data[0..4].try_into().unwrap()) as usize
}

/// Record count of a vec file on disk.
fn read_vec_count(path: &Path) -> Result<usize, String> {
    Ok(fvec_count(&read_raw(path)?))
}
