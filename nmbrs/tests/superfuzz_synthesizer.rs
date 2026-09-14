// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Workload-plane superfuzz: probabilistically SYNTHESIZE whole
//! workload documents across the model's primitives — params,
//! binding classes, phase knobs, stdout and testkit ops
//! (captures, result projections, memos, conditions), poll
//! phases, daemons, scenario iteration / `set:` shadows /
//! includes, and report blocks — restricted to what runs with NO
//! external test system (stdout/testkit adapters only). Each
//! synthesized workload runs through the real binary in a
//! sandbox; the invariant is the system-level twin of the
//! compiler fuzz: **run cleanly or fail cleanly** — a structured
//! error is fine (the synthesizer deliberately emits some
//! invalid documents), but a raw panic, a hang, or a silent
//! failure is a violation.
//!
//! Companion to polydat's `superfuzz_sampler` (GK compile
//! plane): the sampler fuzzes programs, this fuzzes the workload
//! composition surface above them.
//!
//! **Vocabulary coverage contract** — the generator's menu is
//! pinned to `nmbrs_workload::vocab` (the enumerable registry the
//! parser itself consumes): `synthesizer_covers_the_vocabulary`
//! fails whenever a vocabulary entry is neither generated nor on
//! the explicit [`NOT_YET_SYNTHESIZED`] list, so new primitives
//! enter the fuzz surface by default instead of drifting away
//! from it.
//!
//! - `synthesizer_smoke` runs a handful per commit (generator
//!   rot protection).
//! - `superfuzz_synthesizer` is the `#[ignore]`d deep sweep:
//!   `cargo test -p nmbrs --test superfuzz_synthesizer -- --ignored`
//!   Tunables: SYNTH_SEED (base), SYNTH_ITERATIONS (default 150).
//!
//! Sandbox discipline per `feedback_tests_no_project_root`.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

// ─── Deterministic RNG (same shape as the polydat sampler) ───────

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1))
    }
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in `0..n` (n > 0).
    fn range(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    /// True with probability `pct`/100.
    fn chance(&mut self, pct: u64) -> bool {
        self.range(100) < pct
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.range(items.len() as u64) as usize]
    }
}

// ─── Coverage ledger ──────────────────────────────────────────────

/// Vocabulary items the synthesizer does NOT yet generate, each
/// with the reason. Every entry here is a conscious deferral —
/// `synthesizer_covers_the_vocabulary` fails when a vocab entry
/// is missing from BOTH the generator's ledger and this list,
/// and also when an entry here BECOMES covered (remove it then).
///
/// COHERENCE AXIOM (docs/guide/construction_model.md): a
/// construction that cannot be represented in the fuzzer is not
/// yet a valid construction; any intended valid construction
/// must be implemented to be discoverable stochastically by
/// synthetic fuzzing. This list is the DEBT REGISTER against
/// that axiom — every entry is a construction whose validity is
/// not yet demonstrated by fuzzing.
const NOT_YET_SYNTHESIZED: &[(&str, &str)] = &[
    (
        "phase:interval",
        "declarative-only today (phase-interval cascade not built)",
    ),
    (
        "phase:throttle",
        "needs a saturating adapter to exercise; covered by throttle_governor e2e",
    ),
    ("phase:repeat", "rides on phase:interval"),
    ("phase:continue_if", "needs bounded pre-entry predicates"),
    ("phase:dimensions", "needs metric-cell label interplay"),
    ("phase:optimize", "needs optimizer axes and objective wires"),
    (
        "phase:tags",
        "selector-only phases need block-op synthesis (SRD-108 Part A)",
    ),
    ("op:name", "implicit — set by the op's map key"),
    ("op:desc", "legacy alias of op:description"),
    ("op:params", "per-adapter param semantics vary"),
    ("op:delay", "needs delay wires"),
    ("op:traverse", "specialized result-traversal config"),
    ("op:measure", "specialized measurement config"),
    ("op:abstract", "needs blueprint/implements pair synthesis"),
    (
        "op:evaluations",
        "needs ground-truth wires over testkit bodies",
    ),
    ("op:daemon", "op-level daemons need bounded-guard synthesis"),
    ("op:daemon_cancel_grace_ms", "rides on op:daemon"),
    ("op:while", "op-level loops need bounded-guard synthesis"),
    ("op:rate", "op-level pacing wrapper"),
    ("scenario:scenarios", "plural alias of scenario include"),
    ("scenario:do_while", "needs bounded-counter synthesis"),
    ("scenario:do_until", "needs bounded-counter synthesis"),
    ("scenario:counter", "rides on do_while / do_until"),
    ("scenario:bindings", "scenario-node bindings"),
    ("poll:require", "needs live metric selectors"),
    (
        "binding:input",
        "kernel-plane declaration, not workload-authored",
    ),
    (
        "evaluations:relevancy",
        "needs ground-truth wires over testkit bodies",
    ),
    ("evaluations:verify", "needs verify field synthesis"),
    (
        "phase:key_metrics",
        "SRD-113 designations must name families the phase actually \
         emits and carry a valid aggregate; a random column -> agg(family) \
         map is a report-time well-formedness error, not a fuzzable shape",
    ),
    (
        "scenario:anchor",
        "SRD-113 anchors are only meaningful under a sweep whose \
         iterations become table rows; rides on comprehension synthesis",
    ),
];

/// The full vocabulary universe, qualified by plane, derived
/// from the construction registry's projections
/// (`nmbrs_workload::vocab`) so additions there are seen here.
fn vocab_universe() -> BTreeSet<String> {
    use nmbrs_workload::vocab as v;
    let mut u = BTreeSet::new();
    for f in v::phase_fields() {
        u.insert(format!("phase:{f}"));
    }
    for f in v::op_model_fields() {
        u.insert(format!("op:{f}"));
    }
    for f in v::scenario_node_keys() {
        u.insert(format!("scenario:{f}"));
    }
    for f in v::phase_poll_fields() {
        u.insert(format!("poll:{f}"));
    }
    for f in v::binding_classes() {
        u.insert(format!("binding:{f}"));
    }
    for f in v::evaluation_kinds() {
        u.insert(format!("evaluations:{f}"));
    }
    // The statement-payload aliases are one construct; the
    // generator always emits the canonical `stmt` form.
    u.insert("op:stmt-payload".to_string());
    u
}

// ─── Synthesis ────────────────────────────────────────────────────

struct Synthesized {
    yaml: String,
    /// True when the generator deliberately included a construct
    /// the loader must REJECT (unknown op field, undeclared
    /// placeholder). Used only for stats — the clean-outcome
    /// invariant is identical either way.
    intended_invalid: bool,
}

/// Compose one workload document, recording every vocabulary
/// construct it emits into `cov`.
fn synthesize(rng: &mut Rng, cov: &mut BTreeSet<String>) -> Synthesized {
    let mut y = String::new();
    let mut intended_invalid = false;
    let hit = |cov: &mut BTreeSet<String>, item: &str| {
        cov.insert(item.to_string());
    };

    let _ = writeln!(y, "description: synthesized workload (superfuzz)");

    // ── params ──
    let n_params = rng.range(3) as usize; // 0..=2
    let mut int_params: Vec<String> = Vec::new();
    if n_params > 0 {
        let _ = writeln!(y, "params:");
        for i in 0..n_params {
            let name = format!("p{i}");
            let _ = writeln!(y, "  {name}: \"{}\"", 1 + rng.range(5));
            int_params.push(name);
        }
    }

    // ── root bindings (literal/derived consts only — no cycle
    // at workload root) ──
    let mut root_wires: Vec<String> = Vec::new();
    if rng.chance(45) {
        let _ = writeln!(y, "bindings: |");
        let n = 1 + rng.range(2);
        for i in 0..n {
            let name = format!("rc{i}");
            match rng.range(3) {
                0 => {
                    let _ = writeln!(y, "  const {name} := {}", rng.range(100));
                    hit(cov, "binding:const");
                }
                1 => {
                    let _ = writeln!(
                        y,
                        "  const {name} := add({}, {})",
                        rng.range(10),
                        rng.range(10)
                    );
                    hit(cov, "binding:const");
                }
                _ => {
                    let _ = writeln!(y, "  const {name} := mod(hash({}), 10)", rng.range(50));
                    hit(cov, "binding:const");
                }
            }
            root_wires.push(name);
        }
        if rng.chance(35) {
            let _ = writeln!(y, "  shared sh0 := {}", rng.range(10));
            hit(cov, "binding:shared");
        }
    }

    // ── phases ──
    let n_phases = 1 + rng.range(3) as usize; // 1..=3
    let mut phase_names: Vec<String> = Vec::new();
    let mut daemon_phases = 0usize;
    let _ = writeln!(y, "phases:");
    for pi in 0..n_phases {
        let pname = format!("ph{pi}");
        let _ = writeln!(y, "  {pname}:");
        hit(cov, "phase:ops");

        // A daemon needs a foreground sibling to bound it; only
        // allow daemons when at least one other phase stays
        // foreground.
        let daemon = n_phases > 1 && daemon_phases + 1 < n_phases && rng.chance(15);
        if daemon {
            daemon_phases += 1;
            let _ = writeln!(y, "    daemon: true");
            let _ = writeln!(y, "    rate: 200");
            hit(cov, "phase:daemon");
            hit(cov, "phase:rate");
        }

        // Poll phase (testkit, self-satisfying predicate most of
        // the time; occasionally unsatisfiable → clean
        // poll_timeout is an acceptable outcome).
        let poll = !daemon && rng.chance(12);

        // Phase-level for_each sweep (small domain, iter var
        // available to phase config and ops).
        let phase_for_each = !daemon && !poll && rng.chance(12);
        if phase_for_each {
            let _ = writeln!(y, "    for_each: \"fv{pi} in 1, 2\"");
            hit(cov, "phase:for_each");
            if rng.chance(50) {
                let _ = writeln!(y, "    loop_scope: clean");
                hit(cov, "phase:loop_scope");
            }
            if rng.chance(50) {
                let _ = writeln!(y, "    iter_scope: inherit");
                hit(cov, "phase:iter_scope");
            }
        }

        // Extent: cursor XOR cycles (both at once is out of scope
        // for the generator).
        let use_cursor = !poll && rng.chance(25);
        if !use_cursor {
            let cycles = if poll { 1 } else { 1 + rng.range(6) };
            let _ = writeln!(y, "    cycles: {cycles}");
            hit(cov, "phase:cycles");
        }
        let concurrency = 1 + rng.range(if poll { 1 } else { 3 });
        let _ = writeln!(y, "    concurrency: {concurrency}");
        hit(cov, "phase:concurrency");

        if rng.chance(20) {
            let _ = writeln!(
                y,
                "    errors: \"{}\"",
                rng.pick(&["count", "count,retry", ".*:counter"])
            );
            hit(cov, "phase:errors");
        }
        if rng.chance(15) {
            let _ = writeln!(y, "    tries: {}", 1 + rng.range(3));
            hit(cov, "phase:tries");
        }
        if rng.chance(15) {
            let _ = writeln!(y, "    timeout: \"20s\"");
            hit(cov, "phase:timeout");
        }
        if rng.chance(8) {
            let _ = writeln!(y, "    error_rate_max: 0.9");
            hit(cov, "phase:error_rate_max");
        }
        if rng.chance(10) {
            let _ = writeln!(y, "    checkpoint: idempotent");
            hit(cov, "phase:checkpoint");
        }
        if rng.chance(10) {
            let _ = writeln!(y, "    status_metrics: [m*]");
            hit(cov, "phase:status_metrics");
        }
        if !poll && rng.chance(15) {
            let _ = writeln!(y, "    stop_when:");
            let _ = writeln!(y, "      - when: \"to_f64(result_success) > 3\"");
            let _ = writeln!(y, "        effect: stop");
            hit(cov, "phase:stop_when");
        }

        // Phase bindings.
        let mut phase_wires: Vec<String> = root_wires.clone();
        let mut binding_lines: Vec<String> = Vec::new();
        if use_cursor {
            binding_lines.push(format!("cursor cur = range(0, {})", 2 + rng.range(5)));
            hit(cov, "binding:cursor");
        }
        let nb = rng.range(3);
        for bi in 0..nb {
            let name = format!("b{pi}_{bi}");
            match rng.range(3) {
                0 => {
                    binding_lines.push(format!("const {name} := {}", rng.range(100)));
                    hit(cov, "binding:const");
                }
                1 => binding_lines.push(format!("{name} := mod(cycle, {})", 2 + rng.range(5))),
                _ => {
                    binding_lines.push(format!(
                        "const {name} := format_u64({}, 6)",
                        rng.range(1000)
                    ));
                    hit(cov, "binding:const");
                }
            }
            phase_wires.push(name);
        }
        let volatile_now = rng.chance(15);
        if volatile_now {
            binding_lines.push(format!("volatile now{pi} := current_epoch_millis()"));
            hit(cov, "binding:volatile");
        }
        if !binding_lines.is_empty() {
            let _ = writeln!(y, "    bindings: |");
            for l in &binding_lines {
                let _ = writeln!(y, "      {l}");
            }
            hit(cov, "phase:bindings");
        }

        // Phase metrics (completion-time gauges over phase scope).
        if volatile_now && rng.chance(60) {
            let _ = writeln!(y, "    metrics:");
            let _ = writeln!(y, "      m{pi}:");
            let _ = writeln!(y, "        kind: gauge");
            let _ = writeln!(y, "        value: \"now{pi} - phase_start\"");
            hit(cov, "phase:metrics");
        }

        if poll {
            let _ = writeln!(y, "    adapter: testkit");
            hit(cov, "phase:adapter");
            let satisfiable = rng.chance(75);
            let _ = writeln!(y, "    poll:");
            if satisfiable {
                let _ = writeln!(y, "      until: \"gate{pi} == 1\"");
                let _ = writeln!(y, "      timeout_ms: 5000");
            } else {
                // Never true — exercises the poll_timeout path,
                // which must fail CLEANLY (a structured
                // poll_timeout error, not a hang).
                let _ = writeln!(y, "      until: \"gate{pi} == 2\"");
                let _ = writeln!(y, "      timeout_ms: 300");
            }
            let _ = writeln!(y, "      interval_ms: 50");
            hit(cov, "phase:poll");
            hit(cov, "poll:until");
            hit(cov, "poll:timeout_ms");
            hit(cov, "poll:interval_ms");
            if rng.chance(50) {
                let _ = writeln!(y, "      max_error_retries: 0");
                hit(cov, "poll:max_error_retries");
            }
            if rng.chance(50) {
                let _ = writeln!(y, "      metric_name: gate{pi}_wait_s");
                hit(cov, "poll:metric_name");
            }
            if rng.chance(50) {
                let _ = writeln!(y, "      on_timeout: error");
                hit(cov, "poll:on_timeout");
            }
            let _ = writeln!(y, "    ops:");
            let _ = writeln!(y, "      read_state:");
            let _ = writeln!(y, "        stmt: \"POLL\"");
            let _ = writeln!(y, "        result-body:");
            let _ = writeln!(y, "          - value: 1");
            let _ = writeln!(y, "        capture:");
            let _ = writeln!(y, "          gate{pi}: \"/0/value\"");
            hit(cov, "op:stmt-payload");
            hit(cov, "op:capture");
            phase_names.push(pname);
            continue;
        }

        // Ordinary ops.
        let n_ops = 1 + rng.range(2) as usize;
        let _ = writeln!(y, "    ops:");
        for oi in 0..n_ops {
            let oname = format!("op{pi}_{oi}");
            let _ = writeln!(y, "      {oname}:");
            let testkit = rng.chance(35);
            if testkit {
                let _ = writeln!(y, "        adapter: testkit");
            }
            if rng.chance(15) {
                let _ = writeln!(y, "        description: synthesized op");
                hit(cov, "op:description");
            }

            // Statement text with 0..2 in-scope interpolations
            // ({cycle} is always in scope at op level).
            let mut stmt = format!("W{pi}-{oi}");
            let mut interp_pool: Vec<String> = phase_wires.clone();
            interp_pool.extend(int_params.iter().cloned());
            interp_pool.push("cycle".to_string());
            if phase_for_each {
                interp_pool.push(format!("fv{pi}"));
            }
            for _ in 0..rng.range(3) {
                let w = rng.pick(&interp_pool).clone();
                let _ = write!(stmt, " {{{w}}}");
            }
            // Deliberate-invalid draw: an UNDECLARED placeholder
            // must produce the named validator error, cleanly.
            if rng.chance(5) {
                stmt.push_str(" {no_such_wire_anywhere}");
                intended_invalid = true;
            }
            let _ = writeln!(y, "        stmt: \"{stmt}\"");
            hit(cov, "op:stmt-payload");

            if testkit {
                // Structured body + capture, sometimes a
                // result-projection and a memo over the captured
                // wire (which requires the extern declaration —
                // the placeholder validator does not recognise
                // bare capture names).
                let _ = writeln!(y, "        result-body:");
                let _ = writeln!(y, "          n: {}", rng.range(9));
                let _ = writeln!(y, "          rows:");
                let _ = writeln!(y, "            - v: 1");
                let _ = writeln!(y, "            - v: 2");
                if rng.chance(60) {
                    let cap = format!("cap{pi}_{oi}");
                    let memo = rng.chance(50);
                    if memo {
                        let _ = writeln!(y, "        bindings: |");
                        let _ = writeln!(y, "          extern {cap}: u64 = 0");
                        hit(cov, "op:bindings");
                        hit(cov, "binding:extern");
                    }
                    let _ = writeln!(y, "        capture:");
                    let _ = writeln!(y, "          {cap}: /n");
                    hit(cov, "op:capture");
                    if memo {
                        let _ = writeln!(y, "        memo:");
                        let _ = writeln!(y, "          after: \"captured {{{cap}}}\"");
                    }
                }
                if rng.chance(40) {
                    let _ = writeln!(y, "        result:");
                    let _ = writeln!(y, "          vals{pi}_{oi}: \"rows[*].v\"");
                    hit(cov, "op:result");
                }
            }

            if rng.chance(15) {
                let _ = writeln!(y, "        if: \"mod(cycle, 2)\"");
                hit(cov, "op:if");
            }
            if rng.chance(10) {
                let _ = writeln!(y, "        tags:");
                let _ = writeln!(y, "          role: t{}", rng.range(3));
                hit(cov, "op:tags");
            }
            if rng.chance(10) {
                let _ = writeln!(y, "        metrics:");
                let _ = writeln!(y, "          mo{pi}_{oi}:");
                let _ = writeln!(y, "            kind: gauge");
                let _ = writeln!(y, "            value: cycle");
                hit(cov, "op:metrics");
            }
            // Deliberate-invalid draw: an unknown op field must be
            // rejected by SRD-30 field hygiene, cleanly.
            if rng.chance(4) {
                let _ = writeln!(y, "        no_such_field_xyz: 1");
                intended_invalid = true;
            }
        }
        phase_names.push(pname);
    }

    // ── scenarios ──
    let _ = writeln!(y, "scenarios:");
    let style = rng.range(5);
    match style {
        // for-iteration wrapper (iter var available to every phase).
        0 => {
            let key = if rng.chance(50) { "for" } else { "for_each" };
            let _ = writeln!(y, "  default:");
            let _ = writeln!(y, "    - {key}: \"it in 1, 2\"");
            let _ = writeln!(y, "      phases:");
            for p in &phase_names {
                let _ = writeln!(y, "        - {p}");
            }
            hit(
                cov,
                if key == "for" {
                    "scenario:for"
                } else {
                    "scenario:for_each"
                },
            );
            hit(cov, "scenario:phases");
        }
        // set: shadow over an existing param (when one exists).
        1 if !int_params.is_empty() => {
            let _ = writeln!(y, "  default:");
            let _ = writeln!(y, "    - set: {{ {}: \"7\" }}", int_params[0]);
            let _ = writeln!(y, "      phases:");
            for p in &phase_names {
                let _ = writeln!(y, "        - {p}");
            }
            hit(cov, "scenario:set");
            hit(cov, "scenario:phases");
        }
        // Scenario include.
        2 => {
            let _ = writeln!(y, "  default:");
            let _ = writeln!(y, "    - scenario: aux");
            let _ = writeln!(y, "  aux:");
            for p in &phase_names {
                let _ = writeln!(y, "    - {p}");
            }
            hit(cov, "scenario:scenario");
        }
        // Cross-product map form.
        3 => {
            let _ = writeln!(y, "  default:");
            let _ = writeln!(y, "    - for_combinations:");
            let _ = writeln!(y, "        cv: \"1, 2\"");
            let _ = writeln!(y, "      phases:");
            for p in &phase_names {
                let _ = writeln!(y, "        - {p}");
            }
            hit(cov, "scenario:for_combinations");
            hit(cov, "scenario:phases");
        }
        // Plain list.
        _ => {
            let _ = writeln!(y, "  default:");
            for p in &phase_names {
                let _ = writeln!(y, "    - {p}");
            }
        }
    }

    // ── report block ──
    if rng.chance(25) {
        let _ = writeln!(y, "report:");
        let _ = writeln!(y, "  synth_section: |");
        let _ = writeln!(y, "    table t0:");
        let _ = writeln!(y, "      query: c: sum(cycles_total)");
    }

    Synthesized {
        yaml: y,
        intended_invalid,
    }
}

// ─── Execution + classification ──────────────────────────────────

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("nmbrs-synth-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create sandbox");
        Self { dir }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

enum Outcome {
    Completed,
    CleanError,
    Violation(String),
}

/// Run one synthesized workload through the real binary with a
/// hang deadline. Success and structured failure are both clean;
/// panic markers, silent failures, and hangs are violations.
fn run_one(sandbox: &Sandbox, yaml: &str, idx: usize) -> Outcome {
    let wl = sandbox.dir.join(format!("synth_{idx}.yaml"));
    std::fs::write(&wl, yaml).expect("write workload");
    let session = sandbox.dir.join(format!("session_{idx}"));

    let mut child = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .current_dir(&sandbox.dir)
        .args([
            "run",
            &format!("workload=synth_{idx}.yaml"),
            "tui=off",
            "--session-path",
        ])
        .arg(&session)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn nmbrs");

    // Hang deadline: every synthesized extent is tiny, so a
    // healthy run is seconds even in a debug build.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => break Some(s),
            None if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    };

    let mut all = String::new();
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read as _;
        let _ = out.read_to_string(&mut all);
    }
    if let Some(mut err) = child.stderr.take() {
        use std::io::Read as _;
        let mut e = String::new();
        let _ = err.read_to_string(&mut e);
        all.push_str(&e);
    }
    // Session log carries the structured record when console
    // output is terse.
    if let Ok(log) = std::fs::read_to_string(session.join("session.log")) {
        all.push_str(&log);
    }

    let Some(status) = status else {
        return Outcome::Violation("HANG: killed at the 60s deadline".to_string());
    };

    let lower = all.to_lowercase();
    for marker in [
        "panicked at",
        "rust_backtrace",
        "internal error",
        "index out of bounds",
        "not yet implemented",
    ] {
        if lower.contains(marker) {
            return Outcome::Violation(format!("output contains '{marker}'"));
        }
    }
    if status.success() {
        return Outcome::Completed;
    }
    // Failure must be NAMED: a structured error line somewhere in
    // the combined output.
    if all.contains("error:") || all.contains("ERR") {
        Outcome::CleanError
    } else {
        Outcome::Violation(format!(
            "exit {:?} with no recognisable error line",
            status.code()
        ))
    }
}

fn run_sweep(seed: u64, iterations: usize, tag: &str) -> (usize, usize, Vec<String>) {
    let sandbox = Sandbox::new(tag);
    let mut rng = Rng::new(seed);
    let mut cov = BTreeSet::new();
    let mut completed = 0usize;
    let mut clean_errors = 0usize;
    let mut violations: Vec<String> = Vec::new();

    for i in 0..iterations {
        let synth = synthesize(&mut rng, &mut cov);
        match run_one(&sandbox, &synth.yaml, i) {
            Outcome::Completed => completed += 1,
            Outcome::CleanError => clean_errors += 1,
            Outcome::Violation(why) => {
                violations.push(format!(
                    "[seed {seed:#x}] workload {i} ({}): {why}\n  \
                     reproduce: SYNTH_SEED={seed} SYNTH_ITERATIONS={} \
                     cargo test -p nmbrs --test superfuzz_synthesizer -- --ignored\n  \
                     yaml:\n{}",
                    if synth.intended_invalid {
                        "intended-invalid"
                    } else {
                        "intended-valid"
                    },
                    i + 1,
                    synth.yaml,
                ));
                if violations.len() >= 8 {
                    violations.push(format!("[seed {seed:#x}] … stopping after 8 violations"));
                    break;
                }
            }
        }
    }
    (completed, clean_errors, violations)
}

/// Per-commit generator-rot protection: a handful of synthesized
/// workloads must classify cleanly.
#[test]
fn synthesizer_smoke() {
    let (completed, clean_errors, violations) = run_sweep(0xC0FFEE, 4, "smoke");
    eprintln!("synthesizer smoke: {completed} completed, {clean_errors} clean errors");
    assert!(
        violations.is_empty(),
        "synthesizer smoke violations:\n\n{}",
        violations.join("\n---\n")
    );
}

/// Per-commit vocabulary drift-guard: pure generation (no binary
/// runs) across enough draws that every probabilistic branch
/// fires; the union of emitted constructs plus the explicit
/// deferral list must equal the vocabulary universe, and nothing
/// on the deferral list may be secretly covered.
#[test]
fn synthesizer_covers_the_vocabulary() {
    let mut rng = Rng::new(0xC0FFEE);
    let mut cov = BTreeSet::new();
    for _ in 0..800 {
        let _ = synthesize(&mut rng, &mut cov);
    }
    let universe = vocab_universe();
    let deferred: BTreeSet<String> = NOT_YET_SYNTHESIZED
        .iter()
        .map(|(k, _)| k.to_string())
        .collect();

    let uncovered: Vec<&String> = universe
        .iter()
        .filter(|k| !cov.contains(*k) && !deferred.contains(*k))
        .collect();
    assert!(
        uncovered.is_empty(),
        "vocabulary constructs neither synthesized nor consciously deferred \
         (teach the generator, or add to NOT_YET_SYNTHESIZED with a reason): \
         {uncovered:?}"
    );

    let stale: Vec<&String> = deferred.iter().filter(|k| cov.contains(*k)).collect();
    assert!(
        stale.is_empty(),
        "NOT_YET_SYNTHESIZED entries are now covered — remove them: {stale:?}"
    );

    let unknown: Vec<&String> = deferred.iter().filter(|k| !universe.contains(*k)).collect();
    assert!(
        unknown.is_empty(),
        "NOT_YET_SYNTHESIZED entries not present in the vocabulary universe \
         (typo, or the vocab entry was removed): {unknown:?}"
    );

    // Emitted constructs must exist in the universe — a ledger
    // key outside it is a typo in the generator.
    let rogue: Vec<&String> = cov.iter().filter(|k| !universe.contains(*k)).collect();
    assert!(
        rogue.is_empty(),
        "generator recorded constructs outside the vocabulary universe: {rogue:?}"
    );
}

/// MANUAL SUPERFUZZ (workload plane) — run deliberately:
///
/// ```text
/// cargo test -p nmbrs --test superfuzz_synthesizer -- --ignored
/// ```
///
/// Tunables: `SYNTH_SEED` (base seed, default 0xC0FFEE),
/// `SYNTH_ITERATIONS` (default 150; each iteration is one full
/// binary run, so budget roughly a second apiece in debug).
#[test]
#[ignore = "manual superfuzz — minutes of runtime; run with `-- --ignored`"]
fn superfuzz_synthesizer() {
    let seed: u64 = std::env::var("SYNTH_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0xC0FFEE);
    let iterations: usize = std::env::var("SYNTH_ITERATIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(150);

    let (completed, clean_errors, violations) = run_sweep(seed, iterations, "sweep");
    eprintln!(
        "superfuzz_synthesizer: {iterations} workloads — {completed} completed, \
         {clean_errors} clean errors, {} violation(s)",
        violations.len()
    );
    // Generator sanity: a synthesizer that only ever produces
    // failing documents has rotted — the sweep must exercise the
    // RUN path, not just the load-error path.
    assert!(
        completed > 0,
        "no synthesized workload completed — the generator has rotted"
    );
    assert!(
        violations.is_empty(),
        "superfuzz_synthesizer violations ({}):\n\n{}",
        violations.len(),
        violations.join("\n---\n")
    );
}
