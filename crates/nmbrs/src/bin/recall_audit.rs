// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `recall-audit` — offline per-q recall comparator for the
//! aligned audit phases (`recall_audit_oracle`, `recall_audit_pvs`).
//!
//! Both phases iterate the SAME `q` range (q=0..N) against
//! label_00's query vectors, so per-q comparison answers the
//! question: "for the same query vector, does PVS legitimately
//! return more of the true top-K than the oracle path?".
//!
//! For each q present in BOTH phases:
//!
//! - Oracle keys are label-local indices. Compare to `gt_oracle[:K]`
//!   directly.
//! - PVS keys are global indices. Translate global → local via
//!   `metadata_content == label` positions (loaded through the
//!   vectordata API — same source the runtime uses).
//! - Compute `recall@K = |returned ∩ gt[:K]| / K` for each path.
//!
//! Reports: per-q oracle vs pvs deltas, aggregate means, and a
//! list of every q where PVS strictly beats Oracle (showing the
//! returned key sets for inspection).
//!
//! Usage:
//!
//! ```text
//! recall-audit --audit-log <path> --dataset <name|path|url> \
//!              [--label N] [--k K]
//! ```

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::ExitCode;

use vectordata::{
    TestDataGroup, catalog::resolver::Catalog, catalog::sources::CatalogSources, open_facet_typed,
};

/// Per-query accumulator: `(oracle_recall, pvs_recall,
/// ground-truth keys, oracle keys, pvs keys)` keyed by query id.
type PerQuery = BTreeMap<u64, (f64, f64, Vec<i64>, Vec<i64>, Vec<i64>)>;

fn main() -> ExitCode {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    println!("recall-audit:");
    println!("  audit-log = {}", cfg.audit_log.display());
    println!("  dataset   = {}", cfg.dataset);
    println!("  label     = {}", cfg.label);
    println!("  K         = {}", cfg.k);

    // Build global → local map (None when metadata != label).
    let global_to_local = match load_global_to_local(&cfg.dataset, cfg.label) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("failed to load metadata_content via catalog: {e}");
            return ExitCode::from(1);
        }
    };
    let label_count = global_to_local.iter().filter(|o| o.is_some()).count();
    println!(
        "  metadata_content==={}: {} positions",
        cfg.label, label_count
    );

    let phases = match parse_audit_log(&cfg.audit_log) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to parse audit log: {e}");
            return ExitCode::from(1);
        }
    };
    println!("  cycles per phase:");
    for (name, cycles) in &phases {
        println!("    {name}: {} cycles", cycles.len());
    }

    let Some(oracle_phase) = phases.get("recall_audit_oracle") else {
        eprintln!("\nno `recall_audit_oracle` cycles found in audit log");
        return ExitCode::from(1);
    };
    let Some(pvs_phase) = phases.get("recall_audit_pvs") else {
        eprintln!("\nno `recall_audit_pvs` cycles found in audit log");
        return ExitCode::from(1);
    };

    let k = cfg.k;
    let mut per_q: PerQuery = BTreeMap::new();
    let mut oracle_acc = (0u64, 0.0f64);
    let mut pvs_acc = (0u64, 0.0f64);

    for (q, oracle_cyc) in oracle_phase {
        let Some(pvs_cyc) = pvs_phase.get(q) else {
            continue;
        };
        let gt: Vec<i64> = oracle_cyc.gt_oracle.iter().take(k).copied().collect();
        let gt_set: HashSet<i64> = gt.iter().copied().collect();

        let oracle_keys = oracle_cyc.body.clone();
        let oracle_local: HashSet<i64> = oracle_keys.iter().copied().collect();
        let oracle_inter = gt_set.intersection(&oracle_local).count();
        let oracle_recall = oracle_inter as f64 / k as f64;

        let pvs_keys = pvs_cyc.body.clone();
        let pvs_local: HashSet<i64> = pvs_keys
            .iter()
            .filter_map(|&g| {
                global_to_local
                    .get(g as usize)
                    .and_then(|o| o.map(|l| l as i64))
            })
            .collect();
        let pvs_inter = gt_set.intersection(&pvs_local).count();
        let pvs_recall = pvs_inter as f64 / k as f64;

        oracle_acc.0 += 1;
        oracle_acc.1 += oracle_recall;
        pvs_acc.0 += 1;
        pvs_acc.1 += pvs_recall;
        per_q.insert(*q, (oracle_recall, pvs_recall, gt, oracle_keys, pvs_keys));
    }

    println!("\n=== aggregate ===");
    if oracle_acc.0 > 0 {
        println!(
            "  oracle  n={:<4} mean={:.4}",
            oracle_acc.0,
            oracle_acc.1 / oracle_acc.0 as f64
        );
    }
    if pvs_acc.0 > 0 {
        println!(
            "  pvs     n={:<4} mean={:.4}",
            pvs_acc.0,
            pvs_acc.1 / pvs_acc.0 as f64
        );
    }

    // Tally winners.
    let mut oracle_wins = 0u64;
    let mut pvs_wins = 0u64;
    let mut ties = 0u64;
    for (o, p, _, _, _) in per_q.values() {
        if o > p {
            oracle_wins += 1;
        } else if p > o {
            pvs_wins += 1;
        } else {
            ties += 1;
        }
    }
    let total = per_q.len();
    println!("\n=== per-q winners (k={k}, {total} aligned cycles) ===");
    println!("  oracle > pvs:  {oracle_wins}");
    println!("  pvs > oracle:  {pvs_wins}");
    println!("  tie:           {ties}");

    if pvs_wins > 0 {
        println!("\n=== q's where PVS strictly beats Oracle (first 10) ===");
        let mut shown = 0;
        for (q, (o, p, gt, ok, pk)) in &per_q {
            if p <= o {
                continue;
            }
            println!("  q={q:>4}  oracle={o:.2}  pvs={p:.2}");
            println!("    gt[:K]:         {gt:?}");
            println!("    oracle returned: {ok:?}");
            // PVS keys translated to label-local
            let pvs_translated: Vec<i64> = pk
                .iter()
                .filter_map(|&g| {
                    global_to_local
                        .get(g as usize)
                        .and_then(|o| o.map(|l| l as i64))
                })
                .collect();
            println!("    pvs (global):    {pk:?}");
            println!("    pvs (local):     {pvs_translated:?}");
            // Identify which true top-K keys PVS captured that
            // Oracle missed — the "smoking gun" for the
            // thermodynamics question.
            let oracle_set: HashSet<i64> = ok.iter().copied().collect();
            let pvs_local_set: HashSet<i64> = pvs_translated.iter().copied().collect();
            let gt_set: HashSet<i64> = gt.iter().copied().collect();
            let pvs_caught: Vec<i64> = gt_set
                .intersection(&pvs_local_set)
                .filter(|x| !oracle_set.contains(*x))
                .copied()
                .collect();
            let oracle_caught: Vec<i64> = gt_set
                .intersection(&oracle_set)
                .filter(|x| !pvs_local_set.contains(*x))
                .copied()
                .collect();
            println!("    gt keys ONLY pvs caught:    {pvs_caught:?}");
            println!("    gt keys ONLY oracle caught: {oracle_caught:?}");
            shown += 1;
            if shown >= 10 {
                break;
            }
        }
        if pvs_wins > 10 {
            println!("  ... and {} more pvs-wins.", pvs_wins - 10);
        }
    }

    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------

struct Config {
    audit_log: PathBuf,
    dataset: String,
    label: u8,
    k: usize,
}

fn parse_args() -> Result<Config, String> {
    let args: Vec<String> = std::env::args().collect();
    let mut audit_log: Option<PathBuf> = None;
    let mut dataset: Option<String> = None;
    let mut label: u8 = 0;
    let mut k: usize = 10;
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--audit-log" => {
                audit_log = Some(PathBuf::from(
                    it.next().ok_or("--audit-log requires a path")?,
                ));
            }
            "--dataset" => {
                dataset = Some(it.next().ok_or("--dataset requires a value")?.clone());
            }
            "--label" => {
                label = it
                    .next()
                    .ok_or("--label requires a value")?
                    .parse()
                    .map_err(|e| format!("invalid --label: {e}"))?;
            }
            "--k" => {
                k = it
                    .next()
                    .ok_or("--k requires a value")?
                    .parse()
                    .map_err(|e| format!("invalid --k: {e}"))?;
            }
            "--help" | "-h" => {
                eprintln!(
                    "usage: recall-audit --audit-log <path> --dataset <name|path|url> \
                     [--label N] [--k K]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown arg: {other}")),
        }
    }
    Ok(Config {
        audit_log: audit_log.ok_or("missing --audit-log")?,
        dataset: dataset.ok_or("missing --dataset")?,
        label,
        k,
    })
}

/// Load `metadata_content` through the vectordata catalog API,
/// returning a `Vec<Option<u32>>` where index G holds `Some(L)`
/// when `metadata_content[G] == label` (L is the running count
/// of matches so far — the label-local index for global G), or
/// `None` when the byte is some other label.
fn load_global_to_local(spec: &str, label: u8) -> Result<Vec<Option<u32>>, String> {
    let catalog = Catalog::of(&CatalogSources::new().configure_default());
    let group: TestDataGroup = catalog
        .open(spec)
        .map_err(|e| format!("catalog open '{spec}': {e}"))?;
    let bytes = {
        let mut found: Option<Vec<u8>> = None;
        for p in group.profile_names() {
            let Some(view) = group.generic_view(&p) else {
                continue;
            };
            if let Ok(reader) = open_facet_typed::<u8>(&view, "metadata_content") {
                let n = reader.count();
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    out.push(reader.get_native(i));
                }
                found = Some(out);
                break;
            }
        }
        found.ok_or_else(|| "no profile exposes `metadata_content`".to_string())?
    };
    let mut local = 0u32;
    Ok(bytes
        .iter()
        .map(|&b| {
            if b == label {
                let l = local;
                local += 1;
                Some(l)
            } else {
                None
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Audit-log parser
//
// Recognises markers of the form:
//   "=== <phase_name> cycle: label_NN q=<Q> ==="
// (e.g. recall_audit_oracle, recall_audit_pvs). For each
// (phase, q) block, captures the `gt_oracle (...)` array line
// and the first `<op>(...) body:` block's keys.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct CycleData {
    gt_oracle: Vec<i64>,
    body: Vec<i64>,
}

fn parse_audit_log(path: &PathBuf) -> Result<BTreeMap<String, BTreeMap<u64, CycleData>>, String> {
    let f = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let reader = BufReader::new(f);
    let mut phases: BTreeMap<String, BTreeMap<u64, CycleData>> = BTreeMap::new();
    let mut cur_phase: Option<String> = None;
    let mut cur_q: Option<u64> = None;
    let mut in_body = false;

    for line in reader.lines() {
        let line = line.map_err(|e| format!("read line: {e}"))?;
        if let Some(payload) = strip_log_prefix(&line) {
            if let Some((phase, q)) = parse_cycle_header(payload) {
                cur_phase = Some(phase);
                cur_q = Some(q);
                in_body = false;
                phases
                    .entry(cur_phase.clone().unwrap())
                    .or_default()
                    .insert(q, CycleData::default());
            } else if let Some(rest) = payload.strip_prefix("gt_oracle ") {
                in_body = false;
                if let Some(arr_start) = rest.find('[') {
                    let arr = parse_int_array(&rest[arr_start..]).unwrap_or_default();
                    if let (Some(ph), Some(q)) = (&cur_phase, cur_q)
                        && let Some(cyc) = phases.get_mut(ph).and_then(|m| m.get_mut(&q))
                    {
                        cyc.gt_oracle = arr;
                    }
                }
            } else if let Some(first_key) = parse_body_header(payload) {
                in_body = true;
                if let (Some(ph), Some(q)) = (&cur_phase, cur_q)
                    && let Some(cyc) = phases.get_mut(ph).and_then(|m| m.get_mut(&q))
                {
                    cyc.body.clear();
                    if let Some(k) = first_key {
                        cyc.body.push(k);
                    }
                }
            } else {
                in_body = false;
            }
        } else if in_body
            && let Ok(k) = line.trim().parse::<i64>()
            && let (Some(ph), Some(q)) = (&cur_phase, cur_q)
            && let Some(cyc) = phases.get_mut(ph).and_then(|m| m.get_mut(&q))
        {
            cyc.body.push(k);
        }
    }
    Ok(phases)
}

fn strip_log_prefix(line: &str) -> Option<&str> {
    for level in ["INF ", "DBG ", "WRN ", "ERR ", "TRC "] {
        if let Some(rest) = line.strip_prefix(level)
            && let Some(payload) = rest.strip_prefix("log_info: ")
        {
            return Some(payload);
        }
    }
    None
}

/// Match `=== <phase> cycle: label_NN q=<Q> ===` and return
/// `(phase, q)`. Phase is anything between `=== ` and ` cycle:`.
fn parse_cycle_header(payload: &str) -> Option<(String, u64)> {
    let rest = payload.strip_prefix("=== ")?;
    let cycle_pos = rest.find(" cycle:")?;
    let phase = rest[..cycle_pos].to_string();
    let after = &rest[cycle_pos + " cycle:".len()..];
    let q_pos = after.find("q=")?;
    let after_q = &after[q_pos + 2..];
    let end = after_q
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after_q.len());
    let q: u64 = after_q[..end].parse().ok()?;
    Some((phase, q))
}

/// Match `<op>(... body:` (oracle_probe body / pvs_probe body)
/// and return the first key on the header line if present.
fn parse_body_header(payload: &str) -> Option<Option<i64>> {
    // Accept the simpler "oracle_probe body:" / "pvs_probe body:" /
    // also the parenthesised "oracle_probe(prepared) body:" form.
    let body_pos = payload.find(" body:")?;
    let head = &payload[..body_pos];
    if !(head.starts_with("oracle_probe") || head.starts_with("pvs_probe")) {
        return None;
    }
    let rest = payload[body_pos + " body:".len()..].trim_start();
    let first = if rest.is_empty() {
        None
    } else {
        rest.parse::<i64>().ok()
    };
    Some(first)
}

fn parse_int_array(s: &str) -> Option<Vec<i64>> {
    let s = s.trim();
    let s = s.strip_prefix('[')?.strip_suffix(']')?;
    s.split(',').map(|t| t.trim().parse::<i64>().ok()).collect()
}
