// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Workload verification — run a workload and check its output against rules
//! embedded in the workload file. Shared by the `nmbrs check` subcommand and the
//! example-walker test, so "how CI checks the examples" and "how a user checks
//! their own workload" are the same code.
//!
//! A verification target is resolved the same way `nmbrs run` resolves
//! `workload=…`: a directory (walk every workload under it), an existing
//! `.yaml`/`.yml` file (run it by path), or a **bundled catalog name** such as
//! `examples/cursors/all_cursor/enumerate` (run it by name, read its rules from the embedded
//! source). So whatever tab-completion offers for `nmbrs check <TAB>` — local
//! files *and* catalog names — checks the same way it runs.
//!
//! Two **equivalent** rule surfaces (a file may use either, or both — their
//! cases combine):
//!
//! 1. **`#@` comment directives** — trailing YAML comments, inert to the
//!    runtime:
//!    ```text
//!    #@ run scenario=enumerate
//!    #@ expect 50 completed, 0 failed
//!    #@ case overload
//!    #@   run concurrency=32 rate=100000
//!    #@   expect-fail error_rate_exceeded
//!    #@ requires backend (needs a live service)
//!    #@ session cwd            (sessions under the sandbox cwd — stick_session)
//!    #@ again phases=probe     (a second invocation, session state preserved)
//!    ```
//! 2. **A `verify:` YAML block** — also inert (the runtime ignores unknown
//!    top-level keys). Three equivalent shapes:
//!    ```yaml
//!    # single case (a directive map)
//!    verify: { run: scenario=enumerate, expect: "50 completed, 0 failed" }
//!    # a list of cases
//!    verify:
//!      - { case: a, run: scenario=a, expect: "..." }
//!      - { case: b, expect-fail: "..." }
//!    # a name-keyed map (key = case name)
//!    verify:
//!      a: { run: scenario=a, expect: "..." }
//!      b: { expect: ["x", "y"] }
//!    ```
//!
//! `expect` / `expect-fail` accept a single regex or a list of regexes. Each
//! must match the run's combined stdout+stderr; `expect-fail` additionally
//! requires a non-zero exit.

use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Default per-case run timeout, in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 90;

/// Keywords recognized inside a `verify:` directive map (vs. case names).
const DIRECTIVE_KEYS: &[&str] = &[
    "run",
    "expect",
    "expect-fail",
    "expect_fail",
    "requires",
    "timeout",
    "case",
];

/// One verification case: an invocation plus the regexes its output must match.
pub struct VerifyCase {
    pub name: String,
    pub run_args: Vec<String>,
    /// SRD-108 examples round-trip — extra invocations of the SAME
    /// workload in the same sandbox after the first run, one arg
    /// list per `#@ again` line, session state PRESERVED between
    /// invocations. Every non-final invocation must succeed; the
    /// final one feeds the expect / expect-fail rules, and the
    /// `expect` regexes match the CONCATENATED output of all
    /// invocations.
    pub again: Vec<Vec<String>>,
    /// `#@ session cwd` — omit the harness's `--session-path`
    /// injection so sessions land under the (throwaway) sandbox
    /// cwd's `sessions/` root. Required for behaviors keyed to
    /// `sessions/latest` (SRD-106 `stick_session`); the sandbox
    /// `sessions/` dir is wiped at case start so cases stay
    /// independent.
    pub session_cwd: bool,
    pub expects: Vec<Regex>,
    pub expect_fails: Vec<Regex>,
    pub timeout: u64,
}

impl VerifyCase {
    fn new(name: impl Into<String>) -> Self {
        VerifyCase {
            name: name.into(),
            run_args: Vec::new(),
            again: Vec::new(),
            session_cwd: false,
            expects: Vec::new(),
            expect_fails: Vec::new(),
            timeout: DEFAULT_TIMEOUT_SECS,
        }
    }
}

/// The parsed verification plan for one workload file.
pub struct VerifyPlan {
    /// `Some(reason)` ⇒ skip this file (e.g. needs external infra).
    pub requires: Option<String>,
    pub cases: Vec<VerifyCase>,
}

impl VerifyPlan {
    /// Parse both rule surfaces from a workload file's text and combine them.
    pub fn parse(src: &str) -> Result<VerifyPlan, String> {
        let mut plan = parse_directives(src)?;
        merge_verify_block(src, &mut plan)?;
        Ok(plan)
    }

    /// True when the file declares no verification rules at all.
    pub fn is_empty(&self) -> bool {
        self.requires.is_none() && self.cases.is_empty()
    }
}

fn compile(v: &str) -> Result<Regex, String> {
    Regex::new(v).map_err(|e| format!("bad regex {v:?}: {e}"))
}

// ── `#@` comment directives ───────────────────────────────────────────────

fn parse_directives(src: &str) -> Result<VerifyPlan, String> {
    let mut requires: Option<String> = None;
    let mut cases: Vec<VerifyCase> = Vec::new();
    let mut cur: Option<VerifyCase> = None;

    for raw in src.lines() {
        let line = raw.trim_start();
        let Some(rest) = line.strip_prefix("#@") else {
            continue;
        };
        let rest = rest.trim();
        let (kw, val) = match rest.split_once(char::is_whitespace) {
            Some((k, v)) => (k.trim_end_matches(':'), v.trim()),
            None => (rest.trim_end_matches(':'), ""),
        };
        macro_rules! case {
            () => {
                cur.get_or_insert_with(|| VerifyCase::new("default"))
            };
        }
        match kw {
            "requires" => requires = Some(val.to_string()),
            "case" => {
                if let Some(c) = cur.take() {
                    cases.push(c);
                }
                cur = Some(VerifyCase::new(val));
            }
            "run" => case!().run_args = val.split_whitespace().map(String::from).collect(),
            "again" => case!()
                .again
                .push(val.split_whitespace().map(String::from).collect()),
            "session" => match val {
                "cwd" => case!().session_cwd = true,
                other => return Err(format!("unknown `#@ session {other}` (only `cwd`)")),
            },
            "expect" => case!().expects.push(compile(val)?),
            "expect-fail" | "expect_fail" => case!().expect_fails.push(compile(val)?),
            "timeout" => {
                case!().timeout = val.parse().map_err(|_| format!("bad timeout {val:?}"))?
            }
            other => return Err(format!("unknown `#@ {other}` directive")),
        }
    }
    if let Some(c) = cur.take() {
        cases.push(c);
    }
    Ok(VerifyPlan { requires, cases })
}

// ── `verify:` YAML block ──────────────────────────────────────────────────

fn merge_verify_block(src: &str, plan: &mut VerifyPlan) -> Result<(), String> {
    // The file may not be a YAML mapping (or may be a `#@`-only file); either
    // way, no `verify:` block to merge.
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(src) else {
        return Ok(());
    };
    let Some(block) = doc.get("verify") else {
        return Ok(());
    };
    match block {
        serde_yaml::Value::Sequence(items) => {
            for (i, item) in items.iter().enumerate() {
                plan.cases.push(case_from_map(item, None, i)?);
            }
        }
        serde_yaml::Value::Mapping(m) => {
            let all_directives = m
                .keys()
                .all(|k| k.as_str().is_some_and(|s| DIRECTIVE_KEYS.contains(&s)));
            if all_directives {
                // A single directive map — either a file-level `requires` skip
                // or one unnamed case.
                if let Some(req) = m.get("requires").and_then(|v| v.as_str()) {
                    plan.requires = Some(req.to_string());
                } else {
                    plan.cases.push(case_from_map(block, None, 0)?);
                }
            } else {
                // A name-keyed map: each key is a case name.
                for (k, v) in m {
                    let name = k.as_str().ok_or("verify: case name must be a string")?;
                    plan.cases.push(case_from_map(v, Some(name), 0)?);
                }
            }
        }
        _ => return Err("verify: must be a map or a list of cases".to_string()),
    }
    Ok(())
}

/// Build a case from a `verify:` entry map. `name_override` (the key in a
/// name-keyed map) wins; else the entry's `case:` field; else `case-<idx>`.
fn case_from_map(
    v: &serde_yaml::Value,
    name_override: Option<&str>,
    idx: usize,
) -> Result<VerifyCase, String> {
    let m = v.as_mapping().ok_or("verify: each case must be a map")?;
    let get = |k: &str| m.get(serde_yaml::Value::from(k));
    let name = name_override
        .map(String::from)
        .or_else(|| get("case").and_then(|x| x.as_str()).map(String::from))
        .unwrap_or_else(|| format!("case-{idx}"));
    let mut case = VerifyCase::new(name);
    if let Some(run) = get("run").and_then(|x| x.as_str()) {
        case.run_args = run.split_whitespace().map(String::from).collect();
    }
    for s in strings_of(get("expect")) {
        case.expects.push(compile(&s)?);
    }
    for s in strings_of(get("expect-fail").or_else(|| get("expect_fail"))) {
        case.expect_fails.push(compile(&s)?);
    }
    if let Some(t) = get("timeout").and_then(|x| x.as_u64()) {
        case.timeout = t;
    }
    Ok(case)
}

/// A scalar string or a sequence of strings → `Vec<String>`.
fn strings_of(v: Option<&serde_yaml::Value>) -> Vec<String> {
    match v {
        Some(serde_yaml::Value::String(s)) => vec![s.clone()],
        Some(serde_yaml::Value::Sequence(items)) => items
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

// ── Run + check ───────────────────────────────────────────────────────────

/// What one verification case produced.
pub enum Outcome {
    Pass,
    Skip(String),
    Fail(String),
}

/// Run one case via `<binary> run workload=… <args> --session-path …` (under
/// an in-process deadline) from `sandbox`, capture combined output, and check
/// the rules.
/// `workload_ref` is whatever goes after `workload=` — an absolute file path
/// or a bundled catalog name; the subprocess resolves it exactly as a normal
/// `nmbrs run` would.
pub fn run_case(
    binary: &Path,
    workload_ref: &str,
    sandbox: &Path,
    label: &str,
    case: &VerifyCase,
) -> Result<(), String> {
    let safe_label = label.replace(['/', ' ', ':'], "_");
    let session = sandbox.join(format!("session-{safe_label}"));
    // Case independence for cwd-session cases: each gets a
    // PRIVATE working directory, so its `sessions/latest` can
    // neither re-attach to a prior case's session nor race a
    // concurrent case's — the check walker drives cases on
    // worker threads, and a shared cwd made multi-invocation
    // re-attach cases fail only in large sweeps.
    let case_cwd;
    let workdir: &Path = if case.session_cwd {
        case_cwd = sandbox.join(format!("cwd-{safe_label}"));
        let _ = std::fs::remove_dir_all(&case_cwd);
        std::fs::create_dir_all(&case_cwd).map_err(|e| format!("create case cwd: {e}"))?;
        &case_cwd
    } else {
        let _ = std::fs::remove_dir_all(&session);
        sandbox
    };

    // The deadline is enforced here rather than by wrapping in the
    // coreutils `timeout` program: that binary isn't a given on
    // macOS, and on Windows `timeout.exe` is the cmd.exe delay
    // command — it can't run a child at all.
    let invoke = |args: &[String]| -> Result<(String, bool, bool), String> {
        use std::io::Read as _;
        use std::process::Stdio;
        let mut cmd = Command::new(binary);
        cmd.arg("run")
            .arg(format!("workload={workload_ref}"))
            .args(args);
        if !case.session_cwd {
            cmd.arg("--session-path").arg(&session);
        }
        let mut child = cmd
            .current_dir(workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn failed: {e}"))?;
        // Drain both pipes on threads so a chatty child can't fill
        // one while we watch the deadline, deadlocking both sides.
        let mut out_pipe = child.stdout.take().expect("stdout piped");
        let mut err_pipe = child.stderr.take().expect("stderr piped");
        let out_h = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out_pipe.read_to_end(&mut buf);
            buf
        });
        let err_h = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err_pipe.read_to_end(&mut buf);
            buf
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(case.timeout);
        let (status, timed_out) = loop {
            if let Some(st) = child.try_wait().map_err(|e| format!("wait failed: {e}"))? {
                break (st, false);
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let st = child.wait().map_err(|e| format!("wait failed: {e}"))?;
                break (st, true);
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        let stdout = out_h.join().unwrap_or_default();
        let stderr = err_h.join().unwrap_or_default();
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        Ok((combined, status.success(), timed_out))
    };

    let (mut combined, mut succeeded, mut timed_out) = invoke(&case.run_args)?;
    for (i, extra) in case.again.iter().enumerate() {
        if !succeeded || timed_out {
            return Err(format!(
                "invocation {} of {} failed before the `again` steps                  completed:
{combined}",
                i + 1,
                case.again.len() + 1
            ));
        }
        let (c, s, to) = invoke(extra)?;
        combined.push_str(&c);
        succeeded = s;
        timed_out = to;
    }
    check_case_output(case, &combined, succeeded, timed_out)
}

/// Check one case's RUN RESULT against its expectations — the run-mechanism-
/// agnostic half of [`run_case`]. `combined` is the merged stdout+stderr the
/// run produced; `succeeded` is its exit success; `timed_out` is the timeout
/// signal. Used both by the subprocess [`run_case`] and by `nmbrs`'s in-process
/// `run_executions`-backed verification (so the SAME `expect` / `expect-fail`
/// rules apply whether examples run as subprocesses or as concurrent in-process
/// executions sharing one session).
pub fn check_case_output(
    case: &VerifyCase,
    combined: &str,
    succeeded: bool,
    timed_out: bool,
) -> Result<(), String> {
    if timed_out {
        return Err(format!("timed out after {}s", case.timeout));
    }
    if case.expect_fails.is_empty() {
        if !succeeded {
            let err = combined
                .lines()
                .find(|l| l.contains("error:") || l.contains("panic"))
                .unwrap_or("(no error line)");
            return Err(format!("run failed (expected success): {err}"));
        }
    } else {
        if succeeded {
            return Err("expected a failure (`expect-fail`) but the run succeeded".to_string());
        }
        for re in &case.expect_fails {
            if !re.is_match(combined) {
                return Err(format!(
                    "expect-fail /{re}/ did not match the failure output"
                ));
            }
        }
    }
    for re in &case.expects {
        if !re.is_match(combined) {
            return Err(format!("expect /{re}/ did not match the output"));
        }
    }
    Ok(())
}

/// Whether a workload is REQUIRED to declare verification rules.
///
/// True for anything under an `examples/` directory: those files are the
/// documented, CI-gated surface, and one arriving without rules is a
/// regression in the documentation itself. Everywhere else rules are
/// optional — an unruled workload is skipped, not failed.
///
/// Path-component matching, not substring: a workload at
/// `/home/me/examples-scratch/w.yaml` is not under `examples/`, while
/// `/repo/examples/optimizer/w.yaml` is.
pub fn requires_verification_rules(run_ref: &str) -> bool {
    std::path::Path::new(run_ref)
        .components()
        .any(|c| c.as_os_str() == "examples")
}

/// Verify a workload from its rule text + run reference: parse the rules, run
/// every case, return one `(label, Outcome)` per case (or a single Skip / Fail
/// for the whole workload). `run_ref` is what to pass as `workload=` — an
/// absolute file path or a catalog name.
pub fn verify_source(
    binary: &Path,
    label_root: &str,
    run_ref: &str,
    rule_text: &str,
    sandbox: &Path,
) -> Vec<(String, Outcome)> {
    let plan = match VerifyPlan::parse(rule_text) {
        Ok(p) => p,
        Err(e) => return vec![(label_root.to_string(), Outcome::Fail(e))],
    };
    if let Some(reason) = plan.requires {
        return vec![(label_root.to_string(), Outcome::Skip(reason))];
    }
    if plan.cases.is_empty() {
        // A workload with no rules is not a failure in general — it is a
        // workload nobody asked to be checked. `nmbrs check <dir>` walks every
        // YAML it finds, and most of those (adapter workloads, operational
        // ones like compaction_demo_derived) exist to be RUN against real
        // infrastructure, not to self-verify; failing them made the walk's
        // result meaningless and trained people to ignore it.
        //
        // Under `examples/` the opposite holds: those files are the
        // documentation CI gates, and one landing without rules is exactly
        // the regression this check exists to catch. So there, silence is a
        // failure.
        return vec![(
            label_root.to_string(),
            if requires_verification_rules(run_ref) {
                Outcome::Fail(
                    "no verification rules — every workload under `examples/` must \
                     declare them (add `#@ expect …` comments or a `verify:` block)"
                        .into(),
                )
            } else {
                Outcome::Skip("no verification rules — nothing to check".into())
            },
        )];
    }
    plan.cases
        .iter()
        .map(|c| {
            let label = format!("{label_root}::{}", c.name);
            let outcome = match run_case(binary, run_ref, sandbox, &label, c) {
                Ok(()) => Outcome::Pass,
                Err(e) => Outcome::Fail(format!("{label}: {e}")),
            };
            (label, outcome)
        })
        .collect()
}

/// Verify one workload file: read it, then run it by its absolute path. The
/// file is read relative to *this* process's cwd, but the run reference is made
/// absolute because each case is launched from a sandbox cwd.
pub fn verify_file(
    binary: &Path,
    label_root: &str,
    workload: &Path,
    sandbox: &Path,
) -> Vec<(String, Outcome)> {
    let src = match std::fs::read_to_string(workload) {
        Ok(s) => s,
        Err(e) => {
            return vec![(
                label_root.to_string(),
                Outcome::Fail(format!("read error: {e}")),
            )];
        }
    };
    let abs = workload
        .canonicalize()
        .unwrap_or_else(|_| workload.to_path_buf());
    verify_source(binary, label_root, &abs.to_string_lossy(), &src, sandbox)
}

/// Where a verification target's rule text and run reference come from. Mirrors
/// `nmbrs run`'s `workload=…` resolution.
pub enum WorkloadSource {
    /// A workload file on disk: run by (absolute) path, rules from the file.
    File(PathBuf),
    /// A bundled catalog workload: run by name, rules from the embedded source.
    Catalog { name: String, source: String },
}

/// Resolve a single workload reference the way `nmbrs run` does: an existing
/// `.yaml`/`.yml` file path first, then a bundled catalog name. `None` if it is
/// neither. Directories are not a single workload — callers handle those
/// separately (via [`verify_path`]).
pub fn resolve_ref(reference: &str) -> Option<WorkloadSource> {
    let p = Path::new(reference);
    if p.is_file() {
        // Absolute so the sandbox-cwd subprocess can still find it.
        return Some(WorkloadSource::File(
            p.canonicalize().unwrap_or_else(|_| p.to_path_buf()),
        ));
    }
    crate::catalog::lookup(reference).map(|w| WorkloadSource::Catalog {
        name: w.name.to_string(),
        source: w.source.to_string(),
    })
}

/// A workload reference's **declared top-level `params:`** (string scalars),
/// following its `extends:` chain — resolved the way `nmbrs run` resolves
/// `workload=…`. Returns `None` if the reference resolves to nothing or has
/// no `params:` block.
///
/// The canonical "what params did the workload declare" accessor. The
/// runner folds these under the CLI params (CLI wins) to form the run's
/// effective params, so a setting works identically whether declared in the
/// workload or passed on the command line. Also used to recognize a
/// **console-owning adapter declared in the workload** (e.g.
/// `params: { adapter: plotter }`) so the dashboard yields to the adapter on
/// a TTY (SRD-41/87); the returned params carry the adapter's display-shaping
/// keys (e.g. stdout's `filename`) so the preference is decided correctly.
pub fn declared_params(reference: &str) -> Option<std::collections::HashMap<String, String>> {
    let merged = match resolve_ref(reference)? {
        // Display-shaping probe only: resolution warnings are
        // dropped HERE because the authoritative load that follows
        // this probe surfaces the identical warnings itself.
        WorkloadSource::File(path) => crate::extends::load_and_merge(&path).ok()?.0,
        WorkloadSource::Catalog { name, .. } => {
            crate::extends::load_and_merge_bundled(crate::catalog::lookup(&name)?)
                .ok()?
                .0
        }
    };
    let doc: serde_yaml::Value = serde_yaml::from_str(&merged).ok()?;
    let params = doc.get("params")?.as_mapping()?;
    let mut out = std::collections::HashMap::new();
    for (k, v) in params {
        let Some(key) = k.as_str() else { continue };
        // Only scalar params shape the display decision; skip nested
        // structures. Numbers / bools render to their lexical form.
        let val = match v {
            serde_yaml::Value::String(s) => s.clone(),
            serde_yaml::Value::Number(n) => n.to_string(),
            serde_yaml::Value::Bool(b) => b.to_string(),
            _ => continue,
        };
        out.insert(key.to_string(), val);
    }
    Some(out)
}

/// Aggregate verification result across one or more files.
#[derive(Default)]
pub struct VerifySummary {
    pub passed: usize,
    pub skipped: Vec<String>,
    pub failures: Vec<String>,
    /// One entry per workload checked, in completion order — the
    /// raw material for an end-of-run "slowest workloads" report.
    pub timings: Vec<WorkloadTiming>,
}

/// A single workload's aggregate outcome, for live progress and the
/// timing report. Coarser than [`Outcome`] (which is per *case*): a
/// workload with any failing case is [`CheckStatus::Fail`]; one with no
/// failures and at least one skip (and no pass) is [`CheckStatus::Skip`];
/// otherwise [`CheckStatus::Pass`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Skip,
    Fail,
}

/// Wall-clock spent checking one workload, with its aggregate status.
#[derive(Debug, Clone)]
pub struct WorkloadTiming {
    pub label: String,
    pub elapsed: Duration,
    pub status: CheckStatus,
}

/// Live verification progress, emitted as workloads start and finish so a
/// caller (e.g. `nmbrs check`) can render an active/pending/done/errors
/// status line. Invoked from worker threads — handlers must be `Sync`.
#[derive(Debug, Clone)]
pub enum CheckProgress {
    /// Discovery finished — `total` workloads will be checked.
    Begin { total: usize },
    /// A workload began running.
    Started { label: String },
    /// A workload finished, with its wall-clock and aggregate status.
    Finished {
        label: String,
        elapsed: Duration,
        status: CheckStatus,
    },
}

/// A progress handler. `&`-shared across verification worker threads.
pub type ProgressFn<'a> = dyn Fn(CheckProgress) + Sync + 'a;

/// No-op progress handler, for callers that only want the summary.
pub fn no_progress(_: CheckProgress) {}

/// Fold a file's per-case outcomes into one [`CheckStatus`].
fn aggregate_status(outcomes: &[(String, Outcome)]) -> CheckStatus {
    let mut saw_pass = false;
    let mut saw_skip = false;
    for (_, o) in outcomes {
        match o {
            Outcome::Fail(_) => return CheckStatus::Fail,
            Outcome::Pass => saw_pass = true,
            Outcome::Skip(_) => saw_skip = true,
        }
    }
    if saw_skip && !saw_pass {
        CheckStatus::Skip
    } else {
        CheckStatus::Pass
    }
}

/// Verify a workload file or every `*.yaml` under a directory (recursively).
/// Files run concurrently (cases within a file are sequential). `progress`
/// is invoked from worker threads as each workload starts and finishes —
/// pass [`no_progress`] for a quiet run.
pub fn verify_path(
    binary: &Path,
    path: &Path,
    sandbox: &Path,
    progress: &ProgressFn,
) -> VerifySummary {
    let _ = std::fs::create_dir_all(sandbox);
    let mut files: Vec<PathBuf> = Vec::new();
    if path.is_dir() {
        collect_yaml(path, &mut files);
        files.sort();
    } else {
        files.push(path.to_path_buf());
    }
    progress(CheckProgress::Begin { total: files.len() });

    let acc: std::sync::Mutex<VerifySummary> = std::sync::Mutex::new(VerifySummary::default());
    // Work-stealing over `files`: a shared atomic cursor each worker pulls from
    // when it frees up. Static chunking serialized the slow demos (settle /
    // servo workloads that dwell on real-time metric windows) behind one chunk
    // while other chunks idled; a shared queue runs them concurrently, so the
    // wall-clock floor is the single slowest file, not a chunk-sum. Workers
    // spend almost all their time blocked on the child `nmbrs` process, so we
    // oversubscribe past core count (2× cores, capped) to overlap the waits.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = files.len().min(
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .saturating_mul(2)
            .clamp(1, 16),
    );
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(f) = files.get(i) else { break };
                    let label = f
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("?")
                        .to_string();
                    // Run the file (a sequence of cases) outside the lock, then
                    // fold its outcomes under a single lock acquisition.
                    progress(CheckProgress::Started {
                        label: label.clone(),
                    });
                    let start = Instant::now();
                    let outcomes = verify_file(binary, &label, f, sandbox);
                    let elapsed = start.elapsed();
                    let status = aggregate_status(&outcomes);
                    let mut g = acc.lock().unwrap();
                    for (lbl, outcome) in outcomes {
                        match outcome {
                            Outcome::Pass => g.passed += 1,
                            Outcome::Skip(r) => g.skipped.push(format!("{lbl}: {r}")),
                            Outcome::Fail(m) => g.failures.push(m),
                        }
                    }
                    g.timings.push(WorkloadTiming {
                        label: label.clone(),
                        elapsed,
                        status,
                    });
                    drop(g);
                    progress(CheckProgress::Finished {
                        label,
                        elapsed,
                        status,
                    });
                }
            });
        }
    });
    let mut sum = acc.into_inner().unwrap();
    sum.skipped.sort();
    sum.failures.sort();
    sum
}

/// Verify a target named the way `nmbrs run` names workloads: a directory (walk
/// every workload under it), an existing workload file, or a bundled catalog
/// name (`examples/cursors/all_cursor/enumerate`, …). This is the `nmbrs check` entry point — so
/// anything the binary can `run` by name, it can `check` by the same name.
pub fn verify_target(
    binary: &Path,
    target: &str,
    sandbox: &Path,
    progress: &ProgressFn,
) -> VerifySummary {
    let p = Path::new(target);
    if p.is_dir() {
        return verify_path(binary, p, sandbox, progress);
    }
    let _ = std::fs::create_dir_all(sandbox);
    // A single named target is one workload; emit the same Begin/Started/
    // Finished lifecycle a directory walk does so progress rendering is
    // uniform, and record its timing for the report.
    progress(CheckProgress::Begin { total: 1 });
    let label = match &resolve_ref(target) {
        Some(WorkloadSource::File(path)) => path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(target)
            .to_string(),
        _ => target.to_string(),
    };
    progress(CheckProgress::Started {
        label: label.clone(),
    });
    let start = Instant::now();
    let cases: Vec<(String, Outcome)> = match resolve_ref(target) {
        Some(WorkloadSource::File(path)) => verify_file(binary, &label, &path, sandbox),
        Some(WorkloadSource::Catalog { name, source }) => {
            verify_source(binary, &name, &name, &source, sandbox)
        }
        None => vec![(
            target.to_string(),
            Outcome::Fail(format!(
                "no such workload '{target}': not a local file, not a directory, and \
                 no bundled workload by that name (try `nmbrs describe workloads --all`).{}",
                crate::suggest::did_you_mean(&crate::suggest::suggest_workloads(target))
            )),
        )],
    };
    let elapsed = start.elapsed();
    let status = aggregate_status(&cases);
    let mut sum = VerifySummary::default();
    for (lbl, outcome) in cases {
        match outcome {
            Outcome::Pass => sum.passed += 1,
            Outcome::Skip(r) => sum.skipped.push(format!("{lbl}: {r}")),
            Outcome::Fail(m) => sum.failures.push(m),
        }
    }
    sum.timings.push(WorkloadTiming {
        label: label.clone(),
        elapsed,
        status,
    });
    progress(CheckProgress::Finished {
        label,
        elapsed,
        status,
    });
    sum
}

/// Every `*.yaml` / `*.yml` workload file under `dir` (recursive),
/// sorted. The discovery half of [`verify_path`], exposed so the
/// in-process example walker (`nmbrs_runtime::verify_in_process`) finds
/// the same files the subprocess walker does.
pub fn collect_workload_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_yaml(dir, &mut files);
    files.sort();
    files
}

fn collect_yaml(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_yaml(&p, out);
        } else if p.extension().is_some_and(|x| x == "yaml" || x == "yml") {
            out.push(p);
        }
    }
}

#[cfg(test)]
mod rules_required_tests {
    use super::requires_verification_rules;

    /// Under `examples/`, rules are the point — a file without them is a
    /// documentation regression, so the check must fail rather than skip.
    #[test]
    fn examples_require_rules() {
        assert!(requires_verification_rules(
            "/repo/examples/optimizer/control.yaml"
        ));
        assert!(requires_verification_rules("examples/w.yaml"));
        assert!(requires_verification_rules(
            "/repo/examples/modules/module_test.yaml"
        ));
    }

    /// Everywhere else rules are optional: adapter and operational workloads
    /// exist to be RUN against real infrastructure, not to self-verify.
    #[test]
    fn other_locations_do_not_require_rules() {
        assert!(!requires_verification_rules(
            "/repo/adapters/cql/workloads/compaction_demo_derived.yaml"
        ));
        assert!(!requires_verification_rules("/tmp/scratch.yaml"));
        assert!(!requires_verification_rules("some_catalog_name"));
    }

    /// Component matching, not substring — a sibling directory whose name
    /// merely starts with "examples" is not the examples tree.
    #[test]
    fn matches_path_components_not_substrings() {
        assert!(!requires_verification_rules(
            "/home/me/examples-scratch/w.yaml"
        ));
        assert!(!requires_verification_rules("/home/me/myexamples/w.yaml"));
        assert!(requires_verification_rules(
            "/home/me/examples/scratch/w.yaml"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_and_yaml_forms_are_equivalent() {
        let comment = "ops: { a: { raw: x } }\n#@ run cycles=3\n#@ expect 0 failed\n";
        let single = "ops: { a: { raw: x } }\nverify: { run: cycles=3, expect: \"0 failed\" }\n";
        let listed =
            "ops: { a: { raw: x } }\nverify:\n  - { run: cycles=3, expect: \"0 failed\" }\n";
        let named =
            "ops: { a: { raw: x } }\nverify:\n  smoke: { run: cycles=3, expect: \"0 failed\" }\n";
        for src in [comment, single, listed, named] {
            let p = VerifyPlan::parse(src).expect("parse");
            assert_eq!(p.cases.len(), 1, "one case for: {src}");
            assert_eq!(p.cases[0].run_args, vec!["cycles=3"], "run for: {src}");
            assert_eq!(p.cases[0].expects.len(), 1, "expect for: {src}");
        }
    }

    #[test]
    fn name_keyed_map_yields_named_cases() {
        let src = "verify:\n  alpha: { run: scenario=a, expect: \"x\" }\n  beta: { expect: [\"y\", \"z\"] }\n";
        let p = VerifyPlan::parse(src).unwrap();
        let names: Vec<&str> = p.cases.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"alpha") && names.contains(&"beta"),
            "names: {names:?}"
        );
        let beta = p.cases.iter().find(|c| c.name == "beta").unwrap();
        assert_eq!(beta.expects.len(), 2);
    }

    #[test]
    fn requires_block_skips_the_file() {
        let p = VerifyPlan::parse("verify: { requires: needs a backend }\n").unwrap();
        assert_eq!(p.requires.as_deref(), Some("needs a backend"));
        assert!(p.cases.is_empty());
    }

    #[test]
    fn comment_and_block_cases_combine() {
        let src = "#@ case fromcomment\n#@   expect a\nverify:\n  fromblock: { expect: b }\n";
        let p = VerifyPlan::parse(src).unwrap();
        let names: Vec<&str> = p.cases.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"fromcomment") && names.contains(&"fromblock"),
            "{names:?}"
        );
    }

    #[test]
    fn resolve_ref_finds_files_and_rejects_unknown_names() {
        // An on-disk file resolves to an absolute `File` source.
        let dir = std::env::temp_dir().join(format!("nmbrs-verify-resolve-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("w.yaml");
        std::fs::write(&file, "ops: { a: { raw: x } }\n").unwrap();
        match resolve_ref(file.to_str().unwrap()) {
            Some(WorkloadSource::File(p)) => assert!(p.is_absolute(), "absolute: {p:?}"),
            other => panic!(
                "expected File, got {}",
                matches!(other, Some(WorkloadSource::Catalog { .. })) as i32
            ),
        }
        // A name that is neither a file nor (in this test process) a bundled
        // workload resolves to nothing — the CLI reports it as not found.
        assert!(resolve_ref("definitely/not/a/workload").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
