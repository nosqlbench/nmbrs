// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-88 — **the** example-tree walker: every workload under
//! `nmbrs/examples/workloads` verified as concurrent in-process executions
//! sharing one session (≤10 at a time), via
//! [`nmbrs_runtime::runner::run_executions`]. This is the CI gate for the
//! bundled examples (it retired the subprocess-per-case walker). It
//! reuses the shared verifier (`nmbrs_workload::verify`): same discovery
//! ([`collect_workload_files`]), same `#@`/`verify:` rules
//! ([`VerifyPlan`]), the **same rule checker** ([`check_case_output`])
//! the `nmbrs check` CLI uses — one `SessionHost`, N executions under it,
//! no subprocess fan-out.
//!
//! Each case becomes one [`ExecutionSpec`]; its op stdout is captured
//! per-execution via a [`CaptureChannel`] and its lifecycle is folded
//! into a per-execution [`RunState`] (via a real `run_state_actor`).
//! The string the rules match against is that execution's op output +
//! its diagnostic log + a synthesised `phases:  C completed, F failed …`
//! rollup built from the RunState phase tally — **the same source and
//! count the subprocess summary prints**, so counts agree by
//! construction (including dynamic loops, where per-iteration callbacks
//! would over-count: a `do_until` body that runs 3× shows 2 completed).
//!
//! In-process, so the adapter / GK inventory the examples use is
//! force-linked (this binary isn't `nmbrs`).

extern crate nmbrs_adapter_plotter;
extern crate nmbrs_adapter_stdout;
extern crate nmbrs_adapter_testkit;
// GK/optimizer inventory the examples reach for: `nmbrs_optimizers`
// registers the optimizer methods (SRD-86) and `nmbrs_metricsql`
// registers the `metricsql*` GK functions. The `nmbrs` binary
// force-links both for the same reason.
extern crate nmbrs_metricsql;
extern crate nmbrs_optimizers;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use nmbrs_runtime::observer::{LogLevel, PhaseProgressUpdate, RunObserver};
use nmbrs_runtime::output_channel::{CaptureChannel, OutputChannel};
use nmbrs_runtime::readouts::builtins::session_summary::labeled_phase_rollup;
use nmbrs_runtime::runner::{ExecutionSpec, run_executions};
use nmbrs_tui::state::{EntryKind, PhaseStatus, PhaseSummary, RunState};
use nmbrs_workload::verify::{
    VerifyCase, VerifyPlan, VerifySummary, check_case_output, collect_workload_files,
};

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("nmbrs-examples-ip-{pid}-{nanos}"));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Per-execution observer: folds the lifecycle into a per-execution
/// [`RunState`] **synchronously** (the phase events fire on the
/// executor's thread, so the tally is complete by the time
/// `run_executions` returns — no actor thread, no drain). The RunState's
/// find-pending-or-append phase bookkeeping is what makes the count
/// match the post-run summary (dynamic loops included). Also captures
/// the diagnostic log for rule matching.
struct RunStateFeedObserver {
    state: Mutex<RunState>,
    logs: Mutex<Vec<String>>,
}
impl RunStateFeedObserver {
    fn new(label: &str) -> Self {
        Self {
            state: Mutex::new(RunState::new("", label, "")),
            logs: Mutex::new(Vec::new()),
        }
    }
    /// `(completed, failed, total)` over the `Phase` entries.
    fn tally(&self) -> (usize, usize, usize) {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let phases = || s.phases.iter().filter(|p| p.kind == EntryKind::Phase);
        let completed = phases()
            .filter(|p| matches!(p.status, PhaseStatus::Completed))
            .count();
        let failed = phases()
            .filter(|p| matches!(p.status, PhaseStatus::Failed(_)))
            .count();
        (completed, failed, phases().count())
    }
    fn lock_state(&self) -> std::sync::MutexGuard<'_, RunState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}
impl RunObserver for RunStateFeedObserver {
    fn scenario_pre_mapped(&self, tree: &nmbrs_runtime::scene_tree::SceneTree) {
        self.lock_state().install_tree(tree.clone());
    }
    fn phase_starting(
        &self,
        scene_node_id: nmbrs_runtime::scene_tree::SceneNodeId,
        name: &str,
        labels: &str,
        ops: usize,
        _cycles: u64,
        _conc: usize,
    ) {
        self.lock_state()
            .set_phase_running(scene_node_id, name, labels, ops);
    }
    fn phase_completed(
        &self,
        scene_node_id: nmbrs_runtime::scene_tree::SceneNodeId,
        name: &str,
        labels: &str,
        duration_secs: f64,
    ) {
        self.lock_state().set_phase_completed(
            scene_node_id,
            name,
            labels,
            duration_secs,
            PhaseSummary::default(),
        );
    }
    fn phase_failed(
        &self,
        scene_node_id: nmbrs_runtime::scene_tree::SceneNodeId,
        name: &str,
        labels: &str,
        error: &str,
    ) {
        self.lock_state()
            .set_phase_failed(scene_node_id, name, labels, error);
    }
    fn phase_progress(&self, _update: &PhaseProgressUpdate) {}
    fn run_finished(&self) {}
    fn log(&self, _level: LogLevel, message: &str) {
        self.logs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(message.to_string());
    }
}

struct Pending {
    label: String,
    abs: String,
    case: VerifyCase,
    cap: Arc<CaptureChannel>,
    obs: Arc<RunStateFeedObserver>,
}

/// THE walker's run-mechanics partition: a case whose directives change HOW
/// it must be invoked (not merely what to match) cannot run as one concurrent
/// execution and is routed to the serial sequenced path. Kept as a named
/// predicate so `verify_directive_parity_with_walker` (default mix) can pin
/// it against the directive grammar.
fn is_sequenced(case: &VerifyCase) -> bool {
    case.session_cwd || !case.again.is_empty()
}

/// Verify every `*.yaml`/`*.yml` under `examples` as concurrent
/// in-process executions sharing one session, ≤`max_concurrent`.
async fn verify_examples_in_process(
    examples: &std::path::Path,
    sandbox: &std::path::Path,
    max_concurrent: usize,
) -> VerifySummary {
    let _ = std::fs::create_dir_all(sandbox);
    let mut summary = VerifySummary::default();

    let mut pending: Vec<Pending> = Vec::new();
    // SRD-106 cases that CANNOT run as one concurrent execution: a
    // `#@ again` case is a SEQUENCE of invocations sharing session
    // state, and a `#@ session cwd` case needs the harness to stand
    // aside from session selection entirely (any `--session-path`
    // defeats the `stick_session` re-attach rung it exists to test).
    // These run serially after the concurrent groups, mirroring the
    // subprocess harness's per-case sandbox + chdir.
    let mut sequenced: Vec<Pending> = Vec::new();
    for f in collect_workload_files(examples) {
        let file_label = f
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        let src = match std::fs::read_to_string(&f) {
            Ok(s) => s,
            Err(e) => {
                summary
                    .failures
                    .push(format!("{file_label}: read error: {e}"));
                continue;
            }
        };
        let plan = match VerifyPlan::parse(&src) {
            Ok(p) => p,
            Err(e) => {
                summary.failures.push(format!("{file_label}: {e}"));
                continue;
            }
        };
        if let Some(reason) = plan.requires {
            summary.skipped.push(format!("{file_label}: {reason}"));
            continue;
        }
        if plan.cases.is_empty() {
            summary
                .failures
                .push(format!("{file_label}: no verification rules"));
            continue;
        }
        let abs = f.canonicalize().unwrap_or(f).to_string_lossy().into_owned();
        for case in plan.cases {
            let label = format!("{file_label}::{}", case.name);
            let obs = Arc::new(RunStateFeedObserver::new(&label));
            let p = Pending {
                label,
                abs: abs.clone(),
                case,
                cap: Arc::new(CaptureChannel::new()),
                obs,
            };
            if is_sequenced(&p.case) {
                sequenced.push(p);
            } else {
                pending.push(p);
            }
        }
    }

    if pending.is_empty() && sequenced.is_empty() {
        return summary;
    }

    // Group the pending executions by the session-tier params they declare
    // (e.g. `metrics_cadence`) — see `runner::session_param_signature`. Each
    // group runs in its OWN session, set up for that group's shared values;
    // groups run SERIALLY, executions WITHIN a group run concurrently under
    // one session. This mirrors how the subprocess `nmbrs check` gives each
    // workload its own session, and is what lets a workload's sub-second
    // cadence actually take effect in-process (cadence is a session-tier
    // service) without forcing every example through one global session.
    let mut groups: std::collections::BTreeMap<Vec<(String, String)>, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, p) in pending.iter().enumerate() {
        let sig = nmbrs_runtime::runner::session_param_signature(&p.abs);
        groups.entry(sig).or_default().push(i);
    }

    for (group_idx, (sig, members)) in groups.iter().enumerate() {
        let session = sandbox.join(format!("session-{group_idx}"));
        let mut session_args: Vec<String> = vec![
            "--session-path".into(),
            session.to_string_lossy().into_owned(),
        ];
        // Apply the group's shared session params to its session setup.
        for (k, v) in sig {
            session_args.push(format!("{k}={v}"));
        }
        // Every group runs its executions concurrently (the serial-isolation
        // workaround is gone). Grouping remains, but only to give each
        // session-param set its own session — it does NOT serialize.
        //
        // KNOWN GAP: the servo examples (`control` / `multiservo`) still
        // cross-talk under same-session concurrency because metric/metricsql
        // queries are not yet *implicitly* qualified by the execution
        // component's dimensional labels (session + exec_id) — today's
        // exec_id scoping is a leak-prone post-filter, not a by-default
        // matcher injection sourced from the execution component. Until that
        // lands, those two go wrong above 1 concurrent execution.
        let group_concurrent = max_concurrent;
        let session_obs: Arc<dyn RunObserver> =
            Arc::new(nmbrs_runtime::concurrent::HeadlessObserver::new());
        let specs: Vec<ExecutionSpec> = members
            .iter()
            .map(|&i| {
                let p = &pending[i];
                let mut args = vec![format!("workload={}", p.abs)];
                args.extend(p.case.run_args.iter().cloned());
                ExecutionSpec {
                    args,
                    observer: p.obs.clone() as Arc<dyn RunObserver>,
                    channel: Some(p.cap.clone() as Arc<dyn OutputChannel>),
                }
            })
            .collect();

        let results = run_executions(&session_args, session_obs, specs, group_concurrent)
            .await
            .expect("run_executions: session setup");

        for (&i, result) in members.iter().zip(results.iter()) {
            let p = &pending[i];
            let (completed, failed, total) = p.obs.tally();
            let pending_phases = total.saturating_sub(completed + failed);
            let rollup = labeled_phase_rollup(completed, failed, pending_phases, total, false);

            let mut parts: Vec<String> = p.cap.op_lines();
            parts.extend(p.cap.log_lines().into_iter().map(|(_lvl, m)| m));
            parts.extend(
                p.obs
                    .logs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .cloned(),
            );
            if let Err(e) = result {
                parts.push(e.clone());
            }
            parts.push(rollup);
            let combined = parts.join("\n");

            match check_case_output(&p.case, &combined, result.is_ok(), false) {
                Ok(()) => summary.passed += 1,
                Err(e) => summary.failures.push(format!("{}: {e}", p.label)),
            }
        }
    }

    // ── Sequenced cases (`#@ again` / `#@ session cwd`) ──────────
    // One at a time, one invocation at a time — invocation N+1 needs
    // invocation N's session state on disk, and a `session cwd` case
    // needs the process cwd moved into a fresh per-case sandbox so
    // the `stick_session` rung's `sessions/latest` discovery (cwd-
    // relative) sees exactly this case's history and nothing else.
    // chdir is process-global; this is safe because these run AFTER
    // every concurrent group has completed and the only other test
    // in this binary (`verify_directive_parity_with_walker`) is pure
    // parsing with no cwd dependence. Mirrors
    // `nmbrs_workload::verify::run_case`'s subprocess shape
    // (per-case sandbox, `current_dir(sandbox)`).
    struct CwdGuard(PathBuf);
    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }
    for (ci, p) in sequenced.iter().enumerate() {
        let case_dir = sandbox.join(format!("seq-{ci}"));
        let _ = std::fs::create_dir_all(&case_dir);
        let session_args: Vec<String> = if p.case.session_cwd {
            Vec::new()
        } else {
            vec![
                "--session-path".into(),
                case_dir.join("s").to_string_lossy().into_owned(),
            ]
        };
        let _cwd = if p.case.session_cwd {
            let prev = std::env::current_dir().expect("read cwd");
            std::env::set_current_dir(&case_dir).expect("enter case sandbox");
            Some(CwdGuard(prev))
        } else {
            None
        };

        let mut invocations: Vec<Vec<String>> = vec![p.case.run_args.clone()];
        invocations.extend(p.case.again.iter().cloned());
        let total = invocations.len();
        let mut last_ok = true;
        let mut early_failure: Option<String> = None;
        for (i, extra) in invocations.into_iter().enumerate() {
            // The SINGLE-RUN entry (`run_with_observer`), not
            // `run_executions`: the `stick_session` rung — the very
            // thing a `session cwd` case exercises — lives in the
            // single-run session resolution, exactly as `nmbrs run`
            // invokes it. `run_executions` sets its session up from
            // host args before any execution and never consults the
            // workload's stick declaration. Scoped through an
            // ExecutionContext so `diag!` lines route task-locally to
            // this case's observer/capture — the process-global
            // observer is a OnceLock the concurrent groups already
            // claimed, and an unscoped run's diagnostics would land
            // there (or nowhere) instead of in the rule-match text.
            let mut args = vec![format!("workload={}", p.abs)];
            args.extend(session_args.iter().cloned());
            args.extend(extra);
            let ctx = nmbrs_runtime::execution_context::ExecutionContext::with_observer_and_channel(
                p.obs.clone() as Arc<dyn RunObserver>,
                p.cap.clone() as Arc<dyn OutputChannel>,
            );
            let result = nmbrs_runtime::execution_context::scope(
                ctx,
                nmbrs_runtime::runner::run_with_observer(
                    &args,
                    p.obs.clone() as Arc<dyn RunObserver>,
                ),
            )
            .await;
            last_ok = result.is_ok();
            if let Err(e) = &result {
                if i + 1 < total {
                    // Same disposition as the subprocess harness: a
                    // failed invocation with `again` steps outstanding
                    // is its own diagnosis, not a rule mismatch.
                    early_failure = Some(format!(
                        "invocation {} of {total} failed before the `again` \
                         steps completed: {e}",
                        i + 1
                    ));
                }
                p.obs
                    .logs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(e.clone());
            }
            if early_failure.is_some() {
                break;
            }
        }

        if let Some(e) = early_failure {
            summary.failures.push(format!("{}: {e}", p.label));
            continue;
        }
        // The rollup reflects the LAST invocation's phase tally (each
        // invocation reinstalls its scene tree); the sequenced cases'
        // rules match runtime lines, not cross-invocation rollups.
        let (completed, failed, total_phases) = p.obs.tally();
        let pending_phases = total_phases.saturating_sub(completed + failed);
        let rollup = labeled_phase_rollup(completed, failed, pending_phases, total_phases, false);
        let mut parts: Vec<String> = p.cap.op_lines();
        parts.extend(p.cap.log_lines().into_iter().map(|(_lvl, m)| m));
        parts.extend(
            p.obs
                .logs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .cloned(),
        );
        parts.push(rollup);
        let combined = parts.join("\n");
        match check_case_output(&p.case, &combined, last_ok, false) {
            Ok(()) => summary.passed += 1,
            Err(e) => summary.failures.push(format!("{}: {e}", p.label)),
        }
    }

    summary.skipped.sort();
    summary.failures.sort();
    summary
}

// Superseded in the DEFAULT test mix by `example_workload_cases` (a
// harness=false target where every discovered workload is its own
// nextest-scheduled test running the real `nmbrs check`). This sweep remains
// the only place the WHOLE example tree runs as concurrent in-process
// executions in one process, so it stays runnable — explicitly:
//   cargo nextest run -p nmbrs --test example_workloads_in_process --run-ignored all
// The shared-session concurrency PROPERTY itself is covered per-run by
// example_workloads_concurrent.rs.
#[ignore = "superseded by example_workload_cases in the default mix; run explicitly for the full in-process sweep"]
#[test]
fn all_example_workloads_match_their_rules_in_process() {
    // Stability knobs (default to the CI shape, overridable for the
    // worker-thread × execution-concurrency sweep):
    //   NMBRS_TEST_WORKER_THREADS — actual tokio worker threads (hardware
    //     parallelism). The cross-over experiment pins this low (e.g. 1).
    //   NMBRS_TEST_CONCURRENCY — max in-flight executions per session group
    //     (cooperative async concurrency, decoupled from worker threads).
    let worker_threads = env_usize("NMBRS_TEST_WORKER_THREADS", 8).max(1);
    let max_concurrent = env_usize("NMBRS_TEST_CONCURRENCY", 10).max(1);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let summary = rt.block_on(async move {
        // The whole `examples/` tree — `workloads/` plus `modules/` (the
        // adjacent-`.polydat` module-resolution demos) — so CI gates exactly
        // what `nmbrs check workload=examples` checks. NMBRS_TEST_EXAMPLES_DIR
        // narrows the sweep to a subtree (e.g. the optimizer group) for fast
        // stability iteration.
        let examples = match std::env::var("NMBRS_TEST_EXAMPLES_DIR") {
            Ok(d) => PathBuf::from(d),
            Err(_) => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("nmbrs")
                .join("examples"),
        };
        assert!(examples.is_dir(), "examples dir missing at {examples:?}");

        let tmp = TempDir::new();
        verify_examples_in_process(&examples, &tmp.path, max_concurrent).await
    });

    for s in &summary.skipped {
        eprintln!("SKIP {s}");
    }
    eprintln!(
        "in-process verify (worker_threads={worker_threads}, concurrency={max_concurrent}): \
         {} passed, {} skipped, {} failed",
        summary.passed,
        summary.skipped.len(),
        summary.failures.len()
    );

    assert!(
        summary.failures.is_empty(),
        "in-process example verification had {} failure(s):\n{}",
        summary.failures.len(),
        summary.failures.join("\n")
    );
    assert!(summary.passed > 0, "expected at least one example to pass");
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// DEFAULT-MIX parity pin: the `#@` directive grammar and this walker must
/// agree on which directives exist and which change run MECHANICS. The full
/// in-process sweep above is `#[ignore]`d, so a grammar extension the walker
/// doesn't implement fails nothing in CI until someone runs the sweep by hand
/// — exactly how `#@ again` / `#@ session cwd` shipped with the sweep's two
/// provenance cases silently failing (2026-08-06). This test runs in the
/// default mix and breaks the moment the two drift.
///
/// Two mechanisms, deliberately layered:
///  1. an EXHAUSTIVE destructure of [`VerifyCase`] (no `..` rest pattern) —
///     adding a field to the grammar refuses to COMPILE this test until the
///     walker author decides how the sweep honors it (concurrent, sequenced,
///     or match-only) and updates both the walker and this pin;
///  2. runtime assertions that each run-mechanics directive routes through
///     [`is_sequenced`] the way the sweep actually consumes it.
#[test]
fn verify_directive_parity_with_walker() {
    let src = "\
ops: { t: { stmt: \"X\" } }\n\
#@ run scenario=a k=1\n\
#@ expect ALPHA\n\
#@ timeout 33\n\
#@ case seq_again\n\
#@   run scenario=b\n\
#@   again phases=probe tag=zzz\n\
#@   expect BETA\n\
#@ case seq_cwd\n\
#@   session cwd\n\
#@   run\n\
#@   expect-fail GAMMA\n\
";
    let plan = VerifyPlan::parse(src).expect("directive snippet parses");
    assert!(plan.requires.is_none());
    assert_eq!(plan.cases.len(), 3, "default + two named cases");

    // 1 — exhaustiveness: every grammar field, named. A new VerifyCase
    // field breaks this destructure at compile time; when it does, teach
    // the SWEEP above how the field changes case execution (does it route
    // to the sequenced path? alter invocation args? only affect output
    // matching?) before extending the pattern here.
    for case in &plan.cases {
        let VerifyCase {
            name: _,
            run_args: _,
            again: _,
            session_cwd: _,
            expects: _,
            expect_fails: _,
            timeout: _,
        } = case;
    }

    // 2 — routing: run-mechanics directives classify exactly as the sweep
    // consumes them.
    let default_case = &plan.cases[0];
    assert_eq!(default_case.run_args, vec!["scenario=a", "k=1"]);
    assert_eq!(default_case.timeout, 33);
    assert!(
        !is_sequenced(default_case),
        "a case with only run/expect/timeout runs in the concurrent groups"
    );

    let again_case = &plan.cases[1];
    assert_eq!(again_case.name, "seq_again");
    assert_eq!(again_case.again, vec![vec!["phases=probe", "tag=zzz"]]);
    assert!(
        is_sequenced(again_case),
        "`#@ again` is a multi-invocation sequence — must route sequenced"
    );

    let cwd_case = &plan.cases[2];
    assert_eq!(cwd_case.name, "seq_cwd");
    assert!(cwd_case.session_cwd);
    assert!(cwd_case.expect_fails.len() == 1 && cwd_case.expects.is_empty());
    assert!(
        is_sequenced(cwd_case),
        "`#@ session cwd` defeats harness session injection — must route sequenced"
    );

    // `#@ requires` is plan-level: the whole file is skipped, no case runs.
    let skipped = VerifyPlan::parse("ops: { t: { stmt: \"X\" } }\n#@ requires backend\n")
        .expect("requires parses");
    assert!(
        skipped.requires.is_some(),
        "`#@ requires` must surface as a plan-level skip"
    );
}
