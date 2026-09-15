// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The `bench polydat` subcommand: benchmark Polydat expressions across all
//! compilation levels, provenance modes, and thread counts.

use polydat::dsl::compile::compile_polydat_to_assembler;
use polydat::kernel::PolydatProgram;
use std::sync::Arc;

/// How a compiled kernel evaluates per cycle.
///
/// Each mode maps to a distinct monomorphic kernel type produced by
/// the compiler. No runtime branching for strategy selection.
#[derive(Clone, Copy)]
enum EvalMode {
    /// No provenance — eval all nodes unconditionally.
    Raw,
    /// Push-side only — per-node dirty tracking. `set_inputs` marks
    /// dependents dirty, `eval` skips clean nodes. No cone guard.
    PushOnly,
    /// Pull-side only — cone guard checks output provenance vs changed
    /// inputs. If cone is clean, skips eval entirely. Within dirty
    /// cones, runs ALL nodes (no per-node skip).
    PullOnly,
    /// Push + pull — cone guard + per-node skip. Full optimization.
    PushPull,
}

/// Resolve weighted output slots from a scenario using a name→slot resolver.
fn resolve_weighted_slots(
    scenario: &BenchScenario,
    resolve: impl Fn(&str) -> Option<usize>,
) -> (Vec<(usize, u32)>, u32) {
    let slots: Vec<(usize, u32)> = scenario
        .pull_weights
        .iter()
        .filter_map(|(name, w)| resolve(name).map(|s| (s, *w)))
        .collect();
    let total: u32 = slots.iter().map(|(_, w)| w).sum();
    (slots, total)
}

// ── Stats ──────────────────────────────────────────────────────

/// Stats from multiple bench iterations.
struct BenchStats {
    min: f64,
    median: f64,
    p99: f64,
    _cv: f64,
}

fn compute_stats(samples: &mut [f64]) -> BenchStats {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = samples.len();
    let min = samples[0];
    let median = if n % 2 == 1 {
        samples[n / 2]
    } else {
        (samples[n / 2 - 1] + samples[n / 2]) / 2.0
    };
    let p99_idx = ((n as f64) * 0.99).ceil() as usize;
    let p99 = samples[p99_idx.min(n - 1)];
    let mean: f64 = samples.iter().sum::<f64>() / n as f64;
    let variance: f64 = samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
    let cv = if mean > 0.0 {
        variance.sqrt() / mean
    } else {
        0.0
    };
    BenchStats {
        min,
        median,
        p99,
        _cv: cv,
    }
}

/// Subtract driver overhead from stats (floor at 0.1).
fn adjust_stats(stats: &BenchStats, overhead: f64) -> BenchStats {
    BenchStats {
        min: (stats.min - overhead).max(0.1),
        median: (stats.median - overhead).max(0.1),
        p99: (stats.p99 - overhead).max(0.1),
        _cv: stats._cv,
    }
}

// ── Scenario / Driver ──────────────────────────────────────────

/// Parsed bench scenario from a .polydat file's `@driver` and `@pull_weights`.
#[derive(Clone)]
struct BenchScenario {
    driver_source: Option<String>,
    driver_outputs: Vec<String>,
    pull_weights: Vec<(String, u32)>,
}

/// Parse `/// @driver`, `/// @selector`, and `/// @pull_weights` from source.
fn parse_bench_annotations(source: &str) -> BenchScenario {
    let mut driver_lines = Vec::new();
    let mut pull_weights = Vec::new();
    let mut current_section = "";

    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed == "/// @driver" {
            current_section = "driver";
            continue;
        } else if trimmed == "/// @selector" {
            current_section = "selector";
            continue;
        } else if trimmed == "/// @pull_weights" {
            current_section = "pull_weights";
            continue;
        } else if trimmed == "/// @graph" {
            current_section = "";
            continue;
        }

        if !trimmed.starts_with("/// ") && !trimmed.starts_with("//") {
            if current_section == "driver"
                || current_section == "selector"
                || current_section == "pull_weights"
            {
                current_section = "";
            }
            continue;
        }

        let content = if let Some(c) = trimmed.strip_prefix("/// ") {
            c
        } else {
            continue;
        };

        match current_section {
            "driver" => {
                if content.contains(":=") {
                    driver_lines.push(content.to_string());
                }
            }
            "pull_weights" => {
                if let Some((name, weight)) = content.split_once(':')
                    && let Ok(w) = weight.trim().parse::<u32>()
                {
                    pull_weights.push((name.trim().to_string(), w));
                }
            }
            _ => {}
        }
    }

    let (driver_source, driver_outputs) = if !driver_lines.is_empty() {
        let src = format!("input meta: u64\n{}", driver_lines.join("\n"));
        let outputs = match polydat::dsl::compile::compile_polydat(&src) {
            Ok(kernel) => kernel
                .program()
                .output_names()
                .iter()
                .filter(|n| !n.starts_with("__"))
                .map(|n| n.to_string())
                .collect(),
            Err(e) => {
                eprintln!("warning: failed to compile @driver: {e}");
                Vec::new()
            }
        };
        (Some(src), outputs)
    } else {
        (None, Vec::new())
    };

    BenchScenario {
        driver_source,
        driver_outputs,
        pull_weights,
    }
}

/// Per-thread driver state: compiled driver kernel + state.
struct DriverState {
    program: Arc<PolydatProgram>,
    state: polydat::kernel::PolydatState,
    output_names: Vec<String>,
}

impl DriverState {
    fn new(scenario: &BenchScenario) -> Option<Self> {
        let source = scenario.driver_source.as_ref()?;
        let kernel = polydat::dsl::compile::compile_polydat(source).ok()?;
        let program = kernel.into_program();
        let state = program.create_state();
        Some(Self {
            program,
            state,
            output_names: scenario.driver_outputs.clone(),
        })
    }

    fn eval(&mut self, meta_cycle: u64, n_inputs: usize) -> Vec<u64> {
        self.state.set_inputs(&[meta_cycle]);
        let mut inputs = vec![0u64; n_inputs];
        for (i, name) in self.output_names.iter().enumerate() {
            if i >= n_inputs {
                break;
            }
            inputs[i] = self.state.pull(&self.program, name).as_u64();
        }
        inputs
    }
}

fn build_inputs_from_driver(
    driver: &mut Option<DriverState>,
    meta_cycle: u64,
    n_inputs: usize,
) -> Vec<u64> {
    if let Some(d) = driver {
        d.eval(meta_cycle, n_inputs)
    } else {
        let mut inputs = vec![0u64; n_inputs];
        if n_inputs > 0 {
            inputs[0] = meta_cycle;
        }
        inputs
    }
}

// ── Weighted slot selection ────────────────────────────────────

/// Deterministic weighted output selection from cycle hash.
fn pick_weighted_slot(
    weighted_slots: &[(usize, u32)],
    total_weight: u32,
    default_slot: usize,
    cycle: u64,
) -> usize {
    if total_weight == 0 {
        return default_slot;
    }
    let h = xxhash_rust::xxh3::xxh3_64(&cycle.to_le_bytes());
    let b = (h % total_weight as u64) as u32;
    let mut acc = 0u32;
    for &(sl, w) in weighted_slots {
        acc += w;
        if b < acc {
            return sl;
        }
    }
    default_slot
}

/// Select output names for a cycle (used by P1 engines).
fn select_outputs(scenario: &BenchScenario, default_output: &str, meta_cycle: u64) -> Vec<String> {
    if scenario.pull_weights.is_empty() {
        return vec![default_output.to_string()];
    }
    let total_weight: u32 = scenario.pull_weights.iter().map(|(_, w)| w).sum();
    if total_weight == 0 {
        return vec![default_output.to_string()];
    }
    let hash = xxhash_rust::xxh3::xxh3_64(&meta_cycle.to_le_bytes());
    let bucket = (hash % total_weight as u64) as u32;
    let mut accum = 0u32;
    for (name, weight) in &scenario.pull_weights {
        accum += weight;
        if bucket < accum {
            return vec![name.clone()];
        }
    }
    vec![default_output.to_string()]
}

// ── Threaded benchmark harness ─────────────────────────────────

/// Run a multi-threaded benchmark where each thread gets a cycle
/// closure built by `make_kernel`. Returns ns/op samples.
fn run_threaded_bench<F>(
    nthreads: usize,
    iters: usize,
    cycles: u64,
    warmup: u64,
    make_kernel: F,
) -> Vec<f64>
where
    F: Fn() -> Option<Box<dyn FnMut(u64) + Send>>,
{
    let total_ops = (cycles * nthreads as u64) as f64;
    let per_thread = cycles;
    let mut samples = Vec::with_capacity(iters);

    for _ in 0..iters {
        let kernels: Vec<_> = (0..nthreads).filter_map(|_| make_kernel()).collect();
        if kernels.len() != nthreads {
            break;
        }

        let barrier = Arc::new(std::sync::Barrier::new(nthreads));
        let elapsed = Arc::new(std::sync::Mutex::new(std::time::Duration::ZERO));
        let kernel_cells: Vec<_> = kernels.into_iter().map(std::sync::Mutex::new).collect();

        std::thread::scope(|s| {
            for (tid, cell) in kernel_cells.iter().enumerate() {
                let barrier = barrier.clone();
                let elapsed = elapsed.clone();
                s.spawn(move || {
                    let mut kernel = cell.lock().unwrap();
                    let base = tid as u64 * per_thread;
                    for c in base..base + warmup {
                        (kernel)(c);
                    }
                    barrier.wait();
                    let start = std::time::Instant::now();
                    for c in base..base + per_thread {
                        (kernel)(c);
                    }
                    let e = start.elapsed();
                    let mut guard = elapsed.lock().unwrap();
                    if e > *guard {
                        *guard = e;
                    }
                });
            }
        });
        let wall = *elapsed.lock().unwrap();
        samples.push(wall.as_nanos() as f64 / total_ops);
    }

    samples
}

// ── Bench arg parsing ──────────────────────────────────────────

/// Parse a range spec: `N`, `start:end:step`, or `start:end*factor`.
fn parse_range(s: &str) -> Vec<u64> {
    if !s.contains(':') {
        return vec![s.parse().unwrap_or(1)];
    }

    let parts: Vec<&str> = s.splitn(2, ':').collect();
    let start: f64 = parts[0].parse().unwrap_or(1.0);
    let rest = parts[1];

    if let Some(pos) = rest.find('*') {
        let end: f64 = rest[..pos].parse().unwrap_or(start);
        let factor: f64 = rest[pos + 1..].parse().unwrap_or(2.0);
        let mut result = Vec::new();
        let mut v = start;
        while v <= end + 0.001 {
            let rounded = v.round() as u64;
            if rounded >= 1 && (result.is_empty() || *result.last().unwrap() != rounded) {
                result.push(rounded);
            }
            v *= factor;
        }
        return result;
    }

    if let Some(pos) = rest.find(':') {
        let end: f64 = rest[..pos].parse().unwrap_or(start);
        let step: f64 = rest[pos + 1..].parse().unwrap_or(1.0);
        let mut result = Vec::new();
        let mut v = start;
        while v <= end + 0.001 {
            result.push(v.round() as u64);
            v += step;
        }
        return result;
    }

    let end: f64 = rest.parse().unwrap_or(start);
    (start as u64..=end as u64).collect()
}

struct BenchArgs {
    exprs: Vec<String>,
    cycles: u64,
    thread_counts: Vec<u64>,
    explain: bool,
    /// Print the SRD-105 lattice report (cones formed, residue,
    /// P2 headroom) for the production (jit=auto) compile.
    cones: bool,
    engine: Option<String>,
    iters: usize,
    provenance: bool,
    compare_provenance: bool,
    /// Show raw / push-only / push+pull breakdown for each level.
    compare_modes: bool,
}

fn parse_bench_args(args: &[String]) -> BenchArgs {
    let mut ba = BenchArgs {
        exprs: Vec::new(),
        cycles: 100_000,
        thread_counts: vec![1],
        explain: false,
        cones: false,
        engine: None,
        iters: 5,
        provenance: true,
        compare_provenance: false,
        compare_modes: false,
    };

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if let Some(val) = arg.strip_prefix("cycles=") {
            ba.cycles = val.parse().unwrap_or(100_000);
        } else if let Some(val) = arg.strip_prefix("threads=") {
            ba.thread_counts = parse_range(val);
        } else if arg == "--cycles" || arg == "-c" {
            i += 1;
            if i < args.len() {
                ba.cycles = args[i].parse().unwrap_or(100_000);
            }
        } else if arg == "--threads" || arg == "-t" {
            i += 1;
            if i < args.len() {
                ba.thread_counts = parse_range(&args[i]);
            }
        } else if let Some(rest) = arg.strip_prefix("--cycles=") {
            ba.cycles = rest.parse().unwrap_or(100_000);
        } else if let Some(rest) = arg.strip_prefix("--threads=") {
            ba.thread_counts = parse_range(rest);
        } else if arg == "--explain" {
            ba.explain = true;
        } else if arg == "--cones" {
            ba.cones = true;
        } else if arg == "--engine" {
            i += 1;
            if i < args.len() {
                ba.engine = Some(args[i].to_string());
            }
        } else if let Some(val) = arg.strip_prefix("--engine=") {
            ba.engine = Some(val.to_string());
        } else if let Some(val) = arg.strip_prefix("engine=") {
            ba.engine = Some(val.to_string());
        } else if let Some(val) = arg.strip_prefix("iters=") {
            ba.iters = val.parse().unwrap_or(5);
        } else if let Some(val) = arg.strip_prefix("--iters=") {
            ba.iters = val.parse().unwrap_or(5);
        } else if arg == "--iters" {
            i += 1;
            if i < args.len() {
                ba.iters = args[i].parse().unwrap_or(5);
            }
        } else if arg == "--no-provenance" {
            ba.provenance = false;
        } else if arg == "--provenance" {
            ba.provenance = true;
        } else if arg == "--compare" {
            ba.compare_provenance = true;
        } else if arg == "--compare-modes" {
            ba.compare_modes = true;
        } else if arg == "--profile" {
            ba.iters = 1;
            if ba.cycles < 1_000_000 {
                ba.cycles = 1_000_000;
            }
            eprintln!("profile mode: 1 iter, {} cycles", ba.cycles);
            eprintln!(
                "  Run with: perf record -g --call-graph dwarf target/release/nmbrs bench Polydat <expr> --profile cycles=N"
            );
            eprintln!(
                "  Then:     perf script | inferno-collapse-perf | inferno-flamegraph > flame.svg"
            );
        } else if arg.starts_with('-') {
            eprintln!("error: unrecognized option '{arg}'");
            eprintln!(
                "  Valid options: cycles=N, threads=RANGE, --cycles N, --threads RANGE, --explain, --cones, --engine=NAME"
            );
            std::process::exit(1);
        } else {
            ba.exprs.push(arg.clone());
        }
        i += 1;
    }

    // Glob expansion — only for `.polydat` path patterns. A bare
    // `*` or `?` is NOT enough: polydat expressions legitimately
    // contain `*` (`cycle * 3`), and files are recognized by their
    // `.polydat` suffix everywhere else (normalize_source), so the
    // same suffix gates globbing.
    let mut expanded: Vec<String> = Vec::new();
    for expr in &ba.exprs {
        if (expr.contains('*') || expr.contains('?')) && expr.ends_with(".polydat") {
            match glob::glob(expr) {
                Ok(paths) => {
                    let mut found = false;
                    for entry in paths.flatten() {
                        expanded.push(entry.display().to_string());
                        found = true;
                    }
                    if !found {
                        eprintln!("warning: glob '{expr}' matched no files");
                    }
                }
                Err(e) => {
                    eprintln!("warning: bad glob pattern '{expr}': {e}");
                    expanded.push(expr.clone());
                }
            }
        } else {
            expanded.push(expr.clone());
        }
    }
    expanded.sort();
    ba.exprs = expanded;
    ba
}

// ── Explain mode ───────────────────────────────────────────────

fn explain_source(source: &str) {
    use polydat::dsl::events::{CompileEvent, CompileEventLog};
    let mut log = CompileEventLog::new();

    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let (bold, dim, reset, green) = if is_tty {
        ("\x1b[1m", "\x1b[2m", "\x1b[0m", "\x1b[32m")
    } else {
        ("", "", "", "")
    };

    println!("{bold}GK Compilation Explain{reset}");
    println!();

    println!("{bold}Source:{reset}");
    for line in source.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with("//") {
            println!("  {dim}{trimmed}{reset}");
        }
    }
    println!();

    let tokens = match polydat::dsl::lexer::lex(source) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: lex failed: {e}");
            return;
        }
    };
    let ast = match polydat::dsl::parser::parse(tokens) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: parse failed: {e}");
            return;
        }
    };
    log.push(CompileEvent::Parsed {
        statements: ast.statements.len(),
    });

    match compile_polydat_to_assembler(source) {
        Err(e) => {
            eprintln!("error: compile failed: {e}");
        }
        Ok(asm) => {
            for name in asm.output_names() {
                log.push(CompileEvent::OutputDeclared {
                    name: name.to_string(),
                });
            }

            match asm.compile_with_log(Some(&mut log)) {
                Ok(kernel) => {
                    let program = kernel.program();
                    let events = log.events();
                    let has_optimizations = events.iter().any(|e| {
                        matches!(
                            e,
                            CompileEvent::ConstantFolded { .. }
                                | CompileEvent::TypeAdapterInserted { .. }
                                | CompileEvent::FusionApplied { .. }
                        )
                    });
                    if has_optimizations {
                        println!("{bold}Optimizations:{reset}");
                        for event in events {
                            match event {
                                CompileEvent::ConstantFolded { node, value } => {
                                    println!("  {green}constant folded:{reset} {node} → {value}")
                                }
                                CompileEvent::TypeAdapterInserted {
                                    from_node,
                                    to_node,
                                    adapter,
                                } => {
                                    println!("  type adapter: {from_node} → {to_node} ({adapter})")
                                }
                                CompileEvent::FusionApplied {
                                    pattern,
                                    nodes_replaced,
                                } => println!(
                                    "  {green}fusion:{reset} {pattern} ({nodes_replaced} nodes merged)"
                                ),
                                _ => {}
                            }
                        }
                        println!();
                    }

                    println!("{bold}Outputs:{reset}");
                    for name in program.output_names() {
                        if let Some((node_idx, _)) = program.resolve_output(name) {
                            let level = program.node_compile_level(node_idx);
                            let level_str = format!("{level:?}");
                            let color = if level_str.contains("3") { green } else { "" };
                            println!("  {name}: {color}{level_str}{reset}");
                        }
                    }
                    println!();

                    println!(
                        "{bold}Summary:{reset} {} nodes, {} outputs, {} constant(s) folded",
                        program.node_count(),
                        program.output_names().len(),
                        kernel.constants_folded
                    );
                    println!();
                }
                Err(e) => {
                    eprintln!("error: assembly failed: {e}");
                }
            }
        }
    }

    // Engine auto-selection. polydat 0.3 resolves `Provenance::Auto`
    // from the graph's shape inside the engine constructor (the 0.2
    // `auto_compile_p3` / `GraphAnalysis` surface is gone, and the
    // resolved graph it analysed is private). The kernel reports the
    // provenance the selector chose (`Kernel::engine`) and what the
    // engine decided for the program (`Kernel::plan`: native
    // segments / closure steps / interpreted nodes).
    if let Ok(asm) = compile_polydat_to_assembler(source) {
        use polydat::{Engine, Provenance};
        match asm.compile_with(Engine::Native(Provenance::Auto)) {
            Ok(kernel) => {
                println!("{bold}Engine Selection:{reset}");
                println!("  → {green}{}{reset} — {}", kernel.engine(), kernel.plan());
                println!();
            }
            Err(refusal) => {
                // Native refused the program; the closure tier runs
                // the same selector over the same graph.
                if let Ok(asm2) = compile_polydat_to_assembler(source)
                    && let Ok(kernel) = asm2.compile_with(Engine::Closures(Provenance::Auto))
                {
                    println!("{bold}Engine Selection (closures):{reset}");
                    println!("  {dim}native refused: {refusal}{reset}");
                    println!("  → {green}{}{reset} — {}", kernel.engine(), kernel.plan());
                    println!();
                }
            }
        }
    }
}

// ── Single-expression bench ────────────────────────────────────

struct ExprResult {
    label: String,
    nodes: usize,
    p1_ns: f64,
    p2_ns: f64,
    hybrid_ns: f64,
    p3_ns: f64,
}

/// Normalize an expression or file path into Polydat source.
fn normalize_source(expr: &str) -> Result<String, String> {
    if expr.ends_with(".polydat") {
        std::fs::read_to_string(expr).map_err(|e| format!("failed to read '{expr}': {e}"))
    } else {
        let expr = expr.replace(';', "\n");
        if expr.contains(":=") {
            let lines: Vec<&str> = expr
                .lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty() && !l.starts_with("//"))
                .collect();
            let mut out_lines = vec!["input cycle: u64".to_string()];
            for (i, line) in lines.iter().enumerate() {
                if line.contains(":=") {
                    out_lines.push(line.to_string());
                } else if i == lines.len() - 1 {
                    out_lines.push(format!("out := {line}"));
                } else {
                    out_lines.push(format!("__expr_{i} := {line}"));
                }
            }
            Ok(out_lines.join("\n"))
        } else {
            Ok(format!("input cycle: u64\nout := {expr}"))
        }
    }
}

/// Find the last binding name from Polydat source (for output slot resolution).
fn last_binding_name(source: &str) -> String {
    source
        .lines()
        .rev()
        .filter_map(|line| {
            let line = line.trim();
            if let Some(pos) = line.find(":=") {
                let target = line[..pos].trim().trim_matches(|c| c == '(' || c == ')');
                target.split(',').next_back().map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .next()
        .unwrap_or_else(|| "out".to_string())
}

/// Bench a single Polydat expression/file. Returns an ExprResult for comparison tables.
fn bench_single_expr(expr: &str, args: &BenchArgs) -> Option<ExprResult> {
    let source = match normalize_source(expr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return None;
        }
    };
    let warmup = 1000u64;
    let cycles = args.cycles;
    let iters = args.iters;
    let provenance = args.provenance;
    let compare_provenance = args.compare_provenance;

    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let (bold, dim, reset, green) = if is_tty {
        ("\x1b[1m", "\x1b[2m", "\x1b[0m", "\x1b[32m")
    } else {
        ("", "", "", "")
    };

    if args.explain {
        explain_source(&source);
    }

    // SRD-105 lattice report: what extraction actually did for the
    // production (jit=auto) form of this expression — cones formed,
    // interpreter residue, and P2 headroom (the standing evidence
    // feed for the P2-at-cone-boundaries decision).
    if args.cones {
        let mut asm = match compile_polydat_to_assembler(&source) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("error: {e}");
                return None;
            }
        };
        asm.set_jit_mode(polydat::JitMode::Auto);
        match asm.compile() {
            Ok(kernel) => {
                println!("{bold}Lattice (jit=auto){reset}");
                print!(
                    "{}",
                    polydat::compile::lattice::lattice_report(kernel.program())
                );
                println!();
            }
            Err(e) => eprintln!("error: {e:?}"),
        }
    }

    let scenario = parse_bench_annotations(&source);

    let mut bench_node_count = 0usize;
    let mut bench_p1_ns = 0.0f64;
    let mut bench_p2_ns = 0.0f64;
    let mut bench_hybrid_ns = 0.0f64;
    let mut bench_p3_ns = 0.0f64;

    // Compact graph summary — structural (jit=off) shape; the
    // --cones lattice section reports the fused production view.
    if let Ok(mut asm) = compile_polydat_to_assembler(&source)
        && let Ok(kernel) = {
            asm.set_jit_mode(polydat::JitMode::Off);
            asm.compile()
        }
    {
        let program = kernel.program();
        bench_node_count = program.node_count();
        let n_inputs = program.input_names().len();
        let n_outputs = program
            .output_names()
            .iter()
            .filter(|n| !n.starts_with("__"))
            .count();
        let n_nodes = program.node_count();
        let n_wires = program.wire_count();
        let avg_deg = program.avg_degree();

        let label = if expr.len() > 50 {
            format!("{}...", &expr[..47])
        } else {
            expr.to_string()
        };
        println!("{bold}{label}{reset}");
        println!(
            "  {dim}{n_nodes} nodes, {n_wires} wires, {avg_deg:.1} avg degree, {n_inputs} inputs, {n_outputs} outputs{reset}"
        );
    }

    // Calibrate driver overhead
    let driver_ns_per_cycle = if scenario.driver_source.is_some() {
        let mut driver = DriverState::new(&scenario);
        if let Some(ref mut d) = driver {
            let n_inputs = if let Ok(asm) = compile_polydat_to_assembler(&source) {
                asm.compile()
                    .map(|k| k.program().input_names().len())
                    .unwrap_or(1)
            } else {
                1
            };
            for c in 0..warmup {
                d.eval(c, n_inputs);
            }
            let mut samples = Vec::with_capacity(iters);
            for _ in 0..iters {
                let start = std::time::Instant::now();
                for c in 0..cycles {
                    d.eval(c, n_inputs);
                }
                let elapsed = start.elapsed();
                samples.push(elapsed.as_nanos() as f64 / cycles as f64);
            }
            let stats = compute_stats(&mut samples);
            println!(
                "  {dim}driver overhead: {:.1}ns/cycle (subtracted from results){reset}",
                stats.min
            );
            stats.min
        } else {
            0.0
        }
    } else {
        0.0
    };

    let prov_label = if provenance {
        "provenance on"
    } else {
        "provenance off"
    };
    println!("  {dim}{cycles} cycles × {iters} iters, {warmup} warmup, {prov_label}{reset}");
    println!();

    for &nthreads in &args.thread_counts {
        let nthreads = nthreads.max(1) as usize;

        if args.thread_counts.len() > 1 {
            println!();
            println!(
                "  {bold}--- {nthreads} thread{} ---{reset}",
                if nthreads == 1 { "" } else { "s" }
            );
        }
        println!(
            "  {bold}{:<16} {:>10} {:>10} {:>10} {:>8} {:>12}{reset}",
            "Level", "min ns", "median ns", "p99 ns", "Speedup", "ops/s"
        );
        println!("  {}", "-".repeat(72));

        let mut p1_ns: Option<f64> = None;

        let run_raw = compare_provenance;
        let run_deplist = args.engine.as_deref().is_none()
            || args.engine.as_deref() == Some("dependent_list")
            || args.engine.as_deref() == Some("all")
            || compare_provenance;
        let run_provscan = args.engine.as_deref() == Some("provenance_scan")
            || args.engine.as_deref() == Some("all");

        // P1 engines — share the same harness, differ only in state constructor
        match compile_polydat_to_assembler(&source) {
            Err(e) => {
                eprintln!("  {bold}compile error:{reset} {e}");
                return None;
            }
            Ok(mut asm) => {
                if let Ok(kernel) = {
                    asm.set_jit_mode(polydat::JitMode::Off);
                    asm.compile()
                } {
                    let program = kernel.program().clone();
                    let output_name = program
                        .output_names()
                        .first()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "out".to_string());
                    let n_inputs = program.input_names().len();

                    // Shared closure builder for all P1 variants
                    let bench_p1 =
                        |create_state: fn(&Arc<PolydatProgram>) -> Box<dyn P1Engine + Send>| {
                            run_threaded_bench(nthreads, iters, cycles, warmup, || {
                                let program = program.clone();
                                let out = output_name.clone();
                                let ni = n_inputs;
                                let sc = scenario.clone();
                                let mut state = create_state(&program);
                                let mut driver = DriverState::new(&sc);
                                Some(Box::new(move |c: u64| {
                                    let iv = build_inputs_from_driver(&mut driver, c, ni);
                                    state.set_inputs(&iv);
                                    let outs = select_outputs(&sc, &out, c);
                                    for o in &outs {
                                        state.pull_discard(&program, o);
                                    }
                                }))
                            })
                        };

                    if run_raw {
                        let mut samples = bench_p1(|p| Box::new(p.create_raw_state()));
                        if !samples.is_empty() {
                            let stats =
                                adjust_stats(&compute_stats(&mut samples), driver_ns_per_cycle);
                            p1_ns = Some(stats.min);
                            println!(
                                "  {:<16} {:>8.1}ns {:>8.1}ns {:>8.1}ns {:>7.1}x {:>12.0}",
                                "P1",
                                stats.min,
                                stats.median,
                                stats.p99,
                                1.0,
                                1e9 / stats.min
                            );
                        }
                    }

                    if run_deplist {
                        let mut samples = bench_p1(|p| Box::new(p.create_state()));
                        if !samples.is_empty() {
                            let stats =
                                adjust_stats(&compute_stats(&mut samples), driver_ns_per_cycle);
                            let label = if compare_provenance {
                                "P1/prov"
                            } else if run_provscan {
                                "P1/dep_list"
                            } else {
                                "P1"
                            };
                            let speedup = p1_ns.map(|base| base / stats.min).unwrap_or(1.0);
                            println!(
                                "  {:<16} {:>8.1}ns {:>8.1}ns {:>8.1}ns {:>7.1}x {:>12.0}",
                                label,
                                stats.min,
                                stats.median,
                                stats.p99,
                                speedup,
                                1e9 / stats.min
                            );
                            if p1_ns.is_none() || stats.min < p1_ns.unwrap() {
                                p1_ns = Some(stats.min);
                            }
                        }
                    }

                    if run_provscan {
                        let mut samples = bench_p1(|p| Box::new(p.create_provscan_state()));
                        if !samples.is_empty() {
                            let stats =
                                adjust_stats(&compute_stats(&mut samples), driver_ns_per_cycle);
                            if p1_ns.is_none() {
                                p1_ns = Some(stats.min);
                            }
                            let speedup = p1_ns.map(|base| base / stats.min).unwrap_or(1.0);
                            let label = if run_deplist { "P1/prov_scan" } else { "P1" };
                            println!(
                                "  {:<16} {:>8.1}ns {:>8.1}ns {:>8.1}ns {:>7.1}x {:>12.0}",
                                label,
                                stats.min,
                                stats.median,
                                stats.p99,
                                speedup,
                                1e9 / stats.min
                            );
                        }
                    }
                }
            }
        }

        // P2 / Hybrid / P3 engines
        let last_binding = last_binding_name(&source);

        let base_levels: Vec<(&str, bool)> = vec![("P2", false), ("Hybrid", true), ("P3", true)];
        struct LevelConfig {
            label: String,
            highlight: bool,
            base: String,
            prov: bool,
            eval_mode: EvalMode,
        }
        let mut levels: Vec<LevelConfig> = Vec::new();
        if args.compare_modes {
            // 4-way: raw, push-only, pull-only, push+pull
            for &(name, _) in &base_levels {
                levels.push(LevelConfig {
                    label: name.to_string(),
                    highlight: false,
                    base: name.to_string(),
                    prov: false,
                    eval_mode: EvalMode::Raw,
                });
                levels.push(LevelConfig {
                    label: format!("{name}/push"),
                    highlight: false,
                    base: name.to_string(),
                    prov: true,
                    eval_mode: EvalMode::PushOnly,
                });
                levels.push(LevelConfig {
                    label: format!("{name}/pull"),
                    highlight: false,
                    base: name.to_string(),
                    prov: true,
                    eval_mode: EvalMode::PullOnly,
                });
                levels.push(LevelConfig {
                    label: format!("{name}/push+pull"),
                    highlight: true,
                    base: name.to_string(),
                    prov: true,
                    eval_mode: EvalMode::PushPull,
                });
            }
        } else if compare_provenance {
            for &(name, hl) in &base_levels {
                levels.push(LevelConfig {
                    label: name.to_string(),
                    highlight: false,
                    base: name.to_string(),
                    prov: false,
                    eval_mode: EvalMode::Raw,
                });
                levels.push(LevelConfig {
                    label: format!("{name}/prov"),
                    highlight: hl,
                    base: name.to_string(),
                    prov: true,
                    eval_mode: EvalMode::PushPull,
                });
            }
        } else {
            for &(name, hl) in &base_levels {
                let label = if !provenance {
                    name.to_string()
                } else {
                    format!("{name}/prov")
                };
                let mode = if provenance {
                    EvalMode::PushPull
                } else {
                    EvalMode::Raw
                };
                levels.push(LevelConfig {
                    label,
                    highlight: hl,
                    base: name.to_string(),
                    prov: provenance,
                    eval_mode: mode,
                });
            }
        }

        for level in &levels {
            let level_name = level.base.as_str();
            let use_prov = level.prov;
            let available = compile_polydat_to_assembler(&source)
                .ok()
                .and_then(|asm| match (level_name, &level.eval_mode) {
                    ("P2", EvalMode::Raw) => asm.try_compile_raw().ok().map(|_| ()),
                    ("P2", EvalMode::PushOnly) => asm.try_compile_push().ok().map(|_| ()),
                    ("P2", EvalMode::PullOnly) => asm.try_compile_pull().ok().map(|_| ()),
                    ("P2", EvalMode::PushPull) => asm.try_compile().ok().map(|_| ()),
                    ("Hybrid", _) => asm.compile_hybrid().ok().map(|_| ()),
                    ("P3", EvalMode::Raw) => asm.try_compile_jit_raw().ok().map(|_| ()),
                    ("P3", EvalMode::PushOnly) => asm.try_compile_jit_push().ok().map(|_| ()),
                    ("P3", EvalMode::PullOnly) => asm.try_compile_jit_pull().ok().map(|_| ()),
                    ("P3", EvalMode::PushPull) => asm.try_compile_jit().ok().map(|_| ()),
                    _ => None,
                })
                .is_some();

            if !available {
                println!("  {dim}{:<16} {:>10}{reset}", level.label, "—");
                continue;
            }

            let eval_mode = level.eval_mode;
            let mut samples = run_threaded_bench(nthreads, iters, cycles, warmup, || {
                let asm = compile_polydat_to_assembler(&source).ok()?;
                let sc = scenario.clone();
                let lb = last_binding.clone();
                build_compiled_kernel(asm, level_name, use_prov, eval_mode, &sc, &lb)
            });

            if samples.is_empty() {
                continue;
            }
            let stats = adjust_stats(&compute_stats(&mut samples), driver_ns_per_cycle);
            let speedup = p1_ns.map(|p| p / stats.min).unwrap_or(0.0);
            let throughput = 1e9 / stats.min;
            if level.highlight {
                println!(
                    "  {green}{:<16}{reset} {:>8.1}ns {:>8.1}ns {:>8.1}ns {:>7.1}x {:>12.0}",
                    level.label, stats.min, stats.median, stats.p99, speedup, throughput
                );
            } else {
                println!(
                    "  {:<16} {:>8.1}ns {:>8.1}ns {:>8.1}ns {:>7.1}x {:>12.0}",
                    level.label, stats.min, stats.median, stats.p99, speedup, throughput
                );
            }
            match level_name {
                "P2" => {
                    if stats.min < bench_p2_ns || bench_p2_ns == 0.0 {
                        bench_p2_ns = stats.min;
                    }
                }
                "Hybrid" => {
                    if stats.min < bench_hybrid_ns || bench_hybrid_ns == 0.0 {
                        bench_hybrid_ns = stats.min;
                    }
                }
                "P3" => {
                    if stats.min < bench_p3_ns || bench_p3_ns == 0.0 {
                        bench_p3_ns = stats.min;
                    }
                }
                _ => {}
            }
        }

        if let Some(p1) = p1_ns {
            bench_p1_ns = p1;
        }
    }

    println!();

    let label = if expr.ends_with(".polydat") {
        std::path::Path::new(expr)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(expr)
            .to_string()
    } else if expr.len() > 25 {
        format!("{}...", &expr[..22])
    } else {
        expr.to_string()
    };

    Some(ExprResult {
        label,
        nodes: bench_node_count,
        p1_ns: bench_p1_ns,
        p2_ns: bench_p2_ns,
        hybrid_ns: bench_hybrid_ns,
        p3_ns: bench_p3_ns,
    })
}

// ── P1 engine trait ────────────────────────────────────────────

/// Trait unifying the three P1 engine types for benchmarking.
/// The pull return value is discarded — we only care about timing.
trait P1Engine {
    fn set_inputs(&mut self, coords: &[u64]);
    fn pull_discard(&mut self, program: &PolydatProgram, name: &str);
}

impl P1Engine for polydat::kernel::PolydatState {
    fn set_inputs(&mut self, coords: &[u64]) {
        self.set_inputs(coords);
    }
    fn pull_discard(&mut self, program: &PolydatProgram, name: &str) {
        let _ = self.pull(program, name);
    }
}

impl P1Engine for polydat::kernel::RawState {
    fn set_inputs(&mut self, coords: &[u64]) {
        self.set_inputs(coords);
    }
    fn pull_discard(&mut self, program: &PolydatProgram, name: &str) {
        let _ = self.pull(program, name);
    }
}

impl P1Engine for polydat::kernel::ProvScanState {
    fn set_inputs(&mut self, coords: &[u64]) {
        self.set_inputs(coords);
    }
    fn pull_discard(&mut self, program: &PolydatProgram, name: &str) {
        let _ = self.pull(program, name);
    }
}

// ── Compiled kernel builder ────────────────────────────────────

/// Build a per-thread closure for P2/Hybrid/P3 benchmarking.
///
/// Each closure captures its own compiled kernel, driver, and
/// weighted-slot state. The returned `FnMut(u64)` evaluates one
/// cycle including input generation and output selection.
fn build_compiled_kernel(
    asm: polydat::compile::assembly::PolydatAssembler,
    level: &str,
    _use_prov: bool,
    eval_mode: EvalMode,
    scenario: &BenchScenario,
    last_binding: &str,
) -> Option<Box<dyn FnMut(u64) + Send>> {
    match level {
        "P2" => {
            // Each EvalMode selects a distinct monomorphic kernel type
            match eval_mode {
                EvalMode::Raw => asm.try_compile_raw().ok().and_then(|mut k| {
                    let default_out = k.resolve_output(last_binding)?;
                    let n = k.coord_count();
                    let ws = resolve_weighted_slots(scenario, |name| k.resolve_output(name));
                    let mut drv = DriverState::new(scenario);
                    Some(Box::new(move |c: u64| {
                        let iv = build_inputs_from_driver(&mut drv, c, n);
                        let slot = pick_weighted_slot(&ws.0, ws.1, default_out, c);
                        let _ = k.eval_for_slot(&iv, slot);
                    }) as Box<dyn FnMut(u64) + Send>)
                }),
                EvalMode::PushOnly => asm.try_compile_push().ok().and_then(|mut k| {
                    let default_out = k.resolve_output(last_binding)?;
                    let n = k.coord_count();
                    let ws = resolve_weighted_slots(scenario, |name| k.resolve_output(name));
                    let mut drv = DriverState::new(scenario);
                    Some(Box::new(move |c: u64| {
                        let iv = build_inputs_from_driver(&mut drv, c, n);
                        let slot = pick_weighted_slot(&ws.0, ws.1, default_out, c);
                        let _ = k.eval_for_slot(&iv, slot);
                    }) as Box<dyn FnMut(u64) + Send>)
                }),
                EvalMode::PullOnly => asm.try_compile_pull().ok().and_then(|mut k| {
                    let default_out = k.resolve_output(last_binding)?;
                    let n = k.coord_count();
                    let ws = resolve_weighted_slots(scenario, |name| k.resolve_output(name));
                    let mut drv = DriverState::new(scenario);
                    Some(Box::new(move |c: u64| {
                        let iv = build_inputs_from_driver(&mut drv, c, n);
                        let slot = pick_weighted_slot(&ws.0, ws.1, default_out, c);
                        let _ = k.eval_for_slot(&iv, slot);
                    }) as Box<dyn FnMut(u64) + Send>)
                }),
                EvalMode::PushPull => asm.try_compile().ok().and_then(|mut k| {
                    let default_out = k.resolve_output(last_binding)?;
                    let n = k.coord_count();
                    let ws = resolve_weighted_slots(scenario, |name| k.resolve_output(name));
                    let mut drv = DriverState::new(scenario);
                    Some(Box::new(move |c: u64| {
                        let iv = build_inputs_from_driver(&mut drv, c, n);
                        let slot = pick_weighted_slot(&ws.0, ws.1, default_out, c);
                        let _ = k.eval_for_slot(&iv, slot);
                    }) as Box<dyn FnMut(u64) + Send>)
                }),
            }
        }
        "Hybrid" => {
            asm.compile_hybrid()
                .ok()
                .and_then(|mut k: polydat::compile::hybrid::HybridKernel| {
                    let default_out = k.resolve_output(last_binding)?;
                    let n = k.coord_count();
                    let ws = resolve_weighted_slots(scenario, |name| k.resolve_output(name));
                    let mut drv = DriverState::new(scenario);
                    Some(Box::new(move |c: u64| {
                        let iv = build_inputs_from_driver(&mut drv, c, n);
                        k.eval(&iv);
                        let slot = pick_weighted_slot(&ws.0, ws.1, default_out, c);
                        let _ = k.get_slot(slot);
                    }) as Box<dyn FnMut(u64) + Send>)
                })
        }
        "P3" => match eval_mode {
            EvalMode::Raw => asm.try_compile_jit_raw().ok().and_then(|mut k| {
                let default_out = k.resolve_output(last_binding)?;
                let n = k.coord_count();
                let ws = resolve_weighted_slots(scenario, |name| k.resolve_output(name));
                let mut drv = DriverState::new(scenario);
                Some(Box::new(move |c: u64| {
                    let iv = build_inputs_from_driver(&mut drv, c, n);
                    let slot = pick_weighted_slot(&ws.0, ws.1, default_out, c);
                    let _ = k.eval_for_slot(&iv, slot);
                }) as Box<dyn FnMut(u64) + Send>)
            }),
            EvalMode::PushOnly => asm.try_compile_jit_push().ok().and_then(|mut k| {
                let default_out = k.resolve_output(last_binding)?;
                let n = k.coord_count();
                let ws = resolve_weighted_slots(scenario, |name| k.resolve_output(name));
                let mut drv = DriverState::new(scenario);
                Some(Box::new(move |c: u64| {
                    let iv = build_inputs_from_driver(&mut drv, c, n);
                    let slot = pick_weighted_slot(&ws.0, ws.1, default_out, c);
                    let _ = k.eval_for_slot(&iv, slot);
                }) as Box<dyn FnMut(u64) + Send>)
            }),
            EvalMode::PullOnly => asm.try_compile_jit_pull().ok().and_then(|mut k| {
                let default_out = k.resolve_output(last_binding)?;
                let n = k.coord_count();
                let ws = resolve_weighted_slots(scenario, |name| k.resolve_output(name));
                let mut drv = DriverState::new(scenario);
                Some(Box::new(move |c: u64| {
                    let iv = build_inputs_from_driver(&mut drv, c, n);
                    let slot = pick_weighted_slot(&ws.0, ws.1, default_out, c);
                    let _ = k.eval_for_slot(&iv, slot);
                }) as Box<dyn FnMut(u64) + Send>)
            }),
            EvalMode::PushPull => asm.try_compile_jit().ok().and_then(|mut k| {
                let default_out = k.resolve_output(last_binding)?;
                let n = k.coord_count();
                let ws = resolve_weighted_slots(scenario, |name| k.resolve_output(name));
                let mut drv = DriverState::new(scenario);
                Some(Box::new(move |c: u64| {
                    let iv = build_inputs_from_driver(&mut drv, c, n);
                    let slot = pick_weighted_slot(&ws.0, ws.1, default_out, c);
                    let _ = k.eval_for_slot(&iv, slot);
                }) as Box<dyn FnMut(u64) + Send>)
            }),
        },
        _ => None,
    }
}

// ── Comparison table ───────────────────────────────────────────

fn print_comparison_table(results: &[ExprResult]) {
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let (bold, _dim, reset, green) = if is_tty {
        ("\x1b[1m", "\x1b[2m", "\x1b[0m", "\x1b[32m")
    } else {
        ("", "", "", "")
    };

    println!();
    println!("{bold}Comparison (ns/cycle){reset}");
    println!(
        "  {bold}{:<20} {:>5} {:>10} {:>10} {:>10} {:>10} {:>7}{reset}",
        "Graph", "Nodes", "P1", "P2", "Hybrid", "P3", "P3/P1"
    );
    println!("  {}", "-".repeat(77));

    let fmt = |ns: f64| -> String {
        if ns == 0.0 {
            "—".to_string()
        } else {
            format!("{ns:.1}")
        }
    };

    for r in results {
        let speedup = if r.p3_ns > 0.0 && r.p1_ns > 0.0 {
            r.p1_ns / r.p3_ns
        } else {
            0.0
        };

        println!(
            "  {:<20} {:>5} {:>10} {:>10} {:>10} {green}{:>10}{reset} {:>6.1}x",
            r.label,
            r.nodes,
            fmt(r.p1_ns),
            fmt(r.p2_ns),
            fmt(r.hybrid_ns),
            fmt(r.p3_ns),
            speedup
        );
    }
    println!();
}

// ── Top-level entry point ──────────────────────────────────────

pub fn bench_command(args: &[String]) {
    let topic = args.first().map(|s| s.as_str()).unwrap_or("");
    if topic != "wiring" {
        eprintln!(
            "Usage: nmbrs bench wiring <expr> [cycles=N] [threads=RANGE] [--explain] [--cones]"
        );
        eprintln!("  Example: nmbrs bench wiring \"hash_range(hash(cycle), 1000)\"");
        eprintln!(
            "  Example: nmbrs bench wiring \"weighted_pick(hash(cycle), \\\"10:0.5;20:0.3\\\")\" threads=1:8*2"
        );
        eprintln!();
        eprintln!("Range syntax: N | start:end:step | start:end*factor");
        return;
    }

    let ba = parse_bench_args(&args[1..]);

    if ba.exprs.is_empty() {
        eprintln!(
            "Usage: nmbrs bench Polydat <expr|file.polydat ...> [cycles=N] [threads=RANGE] [--explain] [--cones]"
        );
        eprintln!("  Example: nmbrs bench Polydat \"hash_range(hash(cycle), 1000)\"");
        eprintln!("  Example: nmbrs bench Polydat tests/bench_graphs/*.polydat --engine=all");
        return;
    }

    let multi = ba.exprs.len() > 1;
    let mut comparison: Vec<ExprResult> = Vec::new();

    for expr in &ba.exprs {
        if let Some(result) = bench_single_expr(expr, &ba) {
            comparison.push(result);
        }
    }

    if multi && !comparison.is_empty() {
        print_comparison_table(&comparison);
    }
}

// ── cli_spec entry ─────────────────────────────────────────

/// `nmbrs bench <topic> …` — micro-benchmarks; topic-internal
/// parser surface. raw_args=true to keep the existing
/// per-topic flag handling. Per-topic specs are an open gap
/// (see `describe::spec` doc).
/// kv forms `parse_bench_args` plucks (bench.rs parser).
const BENCH_KV: &[crate::cli_spec::KvParam] = &[
    crate::completion::CYCLES_KV_PARAM,
    crate::completion::THREADS_KV_PARAM,
    crate::cli_spec::KvParam {
        key: "engine=",
        provider: crate::completion::free_form,
    },
    crate::cli_spec::KvParam {
        key: "iters=",
        provider: crate::completion::free_form,
    },
];

/// The flag surface `parse_bench_args` consumes — declared so help,
/// completion, and the closed-surface check all see it.
fn bench_flags() -> Vec<crate::cli_spec::Flag> {
    use crate::cli_spec::{Arity, Flag, ValueProvider};
    let b = |long: &'static str, help: &'static str| Flag {
        long,
        short: None,
        aliases: &[],
        arity: Arity::Bool,
        value: ValueProvider::None,
        help,
        repeatable: false,
    };
    vec![
        Flag {
            long: "--cycles",
            short: Some("-c"),
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "Cycles per iteration.",
            repeatable: false,
        },
        Flag {
            long: "--threads",
            short: Some("-t"),
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "Worker threads.",
            repeatable: false,
        },
        Flag {
            long: "--engine",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "Engine to bench (or `all`).",
            repeatable: false,
        },
        Flag {
            long: "--iters",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "Fixed iteration count.",
            repeatable: false,
        },
        b("--explain", "Print the compiled plan before running."),
        b("--cones", "Include cone-shaped workloads."),
        b("--compare", "Compare engines side by side."),
        b("--compare-modes", "Compare execution modes."),
        b("--profile", "Emit profiling output."),
        b("--provenance", "Show value provenance."),
        b("--no-provenance", "Suppress value provenance."),
    ]
}

pub fn spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle(p: ParsedCommand) -> Result<(), String> {
        bench_command(&p.raw);
        Ok(())
    }
    Command {
        name: "bench",
        help: "Micro-benchmarks (`bench <topic>`).",
        category: Category::Benchmark,
        level: Level::FullSurface,
        flags: bench_flags(),
        kv_params: BENCH_KV,
        dynamic_options: None,
        positionals: vec![crate::cli_spec::Positional {
            name: "topic",
            help: "Benchmark topic.",
            kind: crate::cli_spec::PositionalKind::One,
            value: crate::cli_spec::ValueProvider::Custom(crate::completion::bench_topic_provider),
        }],
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: true,
        completion_override: None,
    }
}
