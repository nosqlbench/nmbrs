// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Shared run pipeline for persona binaries.
//!
//! Encapsulates workload parsing → Polydat compilation → activity
//! construction → execution (single or phased).
//!
//! Each persona binary links its adapter crates (which register
//! themselves via `inventory::submit!`) and calls [`run()`].
//! The persona adds nothing but adapters and node functions —
//! all orchestration logic lives here.

use std::collections::HashMap;
use std::sync::Arc;

use crate::activity::Activity;
use crate::adapter::{
    find_adapter_registration, registered_adapter_params, registered_driver_names,
};
use crate::bindings::build_workload_root_kernel;
use crate::opseq::SequencerType;
use crate::synthesis::OpBuilder;
use nmbrs_metrics::labels::Labels;
use nmbrs_metrics::scheduler::Reporter;
use nmbrs_workload::tags::TagFilter;

/// The run-style `key=value` param vocabulary, injected by the CLI layer
/// from its own command-spec (`nmbrs::completion::RUN_KV_PARAMS`) so there is
/// ONE source of truth and zero hand-synced copies. `None` until installed
/// (library/test consumers that drive the runner directly without the CLI):
/// in that case the param-vocabulary validations below are skipped — those
/// are a CLI-surface concern, and the binary always installs the list before
/// any run. See [`install_known_params`] / [`known_params`].
static KNOWN_PARAMS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();

/// Install the run-style param vocabulary (the keys the CLI command-spec
/// accepts, sans the trailing `=`). Called once at binary startup from the
/// CLI layer, which owns the canonical list. Idempotent — first wins.
pub fn install_known_params(keys: Vec<&'static str>) {
    let _ = KNOWN_PARAMS.set(keys);
}

/// The installed param vocabulary, or `None` when no CLI layer registered
/// one. Validators treat `None` as "skip the closed-vocabulary check" so a
/// direct library/test driver isn't held to the CLI's param surface.
/// Adapter-specific params are still discovered from inventory regardless.
fn known_params() -> Option<&'static [&'static str]> {
    KNOWN_PARAMS.get().map(|v| v.as_slice())
}

/// Whether `name` is an installed CLI param key. When no vocabulary is
/// installed (library/test driver), every name is treated as known so the
/// closed-vocabulary validations no-op rather than false-reject a workload.
pub(crate) fn is_cli_param(name: &str) -> bool {
    known_params().map(|p| p.contains(&name)).unwrap_or(true)
}

/// Spec-derived flag vocabulary (SRD-15 CLI substrate): the run command's
/// DECLARED flags, split by arity, installed at binary startup exactly like
/// [`install_known_params`]. Before this, [`parse_params`] validated argv
/// against its own hardcoded lists ([`RECOGNIZED_BARE_FLAGS`] /
/// [`SESSION_DIR_FLAGS`]), which had drifted from the spec — declared flags
/// like `--no-prompt` (bool) and the space forms of `--jit` / `--kernel-opt`
/// / declared aliases like `--session-dir` were hard-rejected while help and
/// completion advertised them. The hardcoded lists remain only as the
/// library/test-driver fallback.
static KNOWN_BARE_FLAGS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
static KNOWN_VALUE_FLAGS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();

/// Install the run-style flag vocabulary (long forms + aliases from the CLI
/// command-spec, split by arity). Idempotent — first wins.
pub fn install_known_flags(bare: Vec<&'static str>, value: Vec<&'static str>) {
    let _ = KNOWN_BARE_FLAGS.set(bare);
    let _ = KNOWN_VALUE_FLAGS.set(value);
}

/// Recognized bare (boolean) flag: the installed spec list, or the
/// hardcoded fallback. `--refine` is runner-internal (injected by the
/// refine command, never user-declared) so it is always recognized.
pub(crate) fn is_recognized_bare_flag(arg: &str) -> bool {
    arg == "--refine"
        || KNOWN_BARE_FLAGS
            .get()
            .map(|v| v.iter().any(|f| *f == arg))
            .unwrap_or_else(|| RECOGNIZED_BARE_FLAGS.contains(&arg))
}

/// Recognized value-taking flag (space form consumes the next token):
/// the installed spec list, or the hardcoded session-flag fallback.
pub(crate) fn known_value_flags() -> &'static [&'static str] {
    KNOWN_VALUE_FLAGS
        .get()
        .map(|v| v.as_slice())
        .unwrap_or(SESSION_DIR_FLAGS)
}

/// A CLI flag's value, accepting `--flag=value` and `--flag value`.
///
/// The same two spellings [`crate::session::resolve_flag`] accepts, minus its
/// environment-variable fallback and conflict check — for flags that are
/// CLI-only by design. Written out because the flags declared in the command
/// spec are advertised (in `--help` and completion) as taking a value, and a
/// reader matching only `--flag=` would silently ignore the spelling the
/// completer suggests.
fn cli_flag_value(args: &[String], flag: &str) -> Option<String> {
    let eq_prefix = format!("{flag}=");
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(rest) = arg.strip_prefix(&eq_prefix) {
            return Some(rest.to_string());
        }
        if arg == flag {
            return iter.next().cloned();
        }
    }
    None
}

/// Session-dir file holding end-of-run summary output that is
/// routed to stdout (SRD-46 `to stdout`) but could not be written
/// inline because a TUI owned the terminal. The post-run printer
/// flushes it verbatim once the terminal is back in cooked mode.
///
/// Dot-prefixed, and deliberately NOT matching the `_summary.`
/// artifact pattern: artifacts are the `sessiondir` destination,
/// and their presence must never by itself imply a stdout write.
pub const DEFERRED_STDOUT_FILE: &str = ".report_stdout.md";

/// Convert the workload-model `SummaryConfig` (parsed from the
/// `summary:` workload field or the `--summary` CLI flag) into
/// the SQLite reporter's `ReportConfig`. Used by both the
/// in-run summary path (workload finished, render to
/// `summary.md`) and the standalone `nmbrs --summary` command,
/// so both produce identical output for the same spec.
pub fn report_config_from_summary(
    config: &nmbrs_workload::model::SummaryConfig,
    exec_id_filter: Option<u64>,
) -> nmbrs_metrics::reporters::sqlite::ReportConfig {
    nmbrs_metrics::reporters::sqlite::ReportConfig {
        columns: config.columns.clone(),
        row_filters: config.row_filters.clone(),
        aggregates: config
            .aggregates
            .iter()
            .map(|a| nmbrs_metrics::reporters::sqlite::ReportAggregate {
                function: a.function.to_string(),
                column_pattern: a.column_pattern.clone(),
                label_key: a.label_key.clone(),
                label_pattern: a.label_pattern.clone(),
                group_by: a.group_by.clone(),
            })
            .collect(),
        show_details: config.show_details,
        exec_id_filter,
    }
}

/// Try to resolve a workload name (bare or with extension) to an
/// actual file path, searching the current directory and
/// `./workloads/`. Returns `None` if nothing matches.
///
/// Exposed for shell-completion tooling. Application code should
/// just use [`run_with_observer`] which calls this internally.
pub fn resolve_workload_file_public(name: &str) -> Option<String> {
    resolve_workload_file(name)
}

/// List the scenario names declared at the top level of a workload
/// YAML file. Used by shell-completion tooling to offer
/// `scenario=<tab>` suggestions. Returns an empty vector on any
/// parse error — completion is best-effort, not a hard check.
pub fn scenarios_in_workload_file(path: &str) -> Vec<String> {
    let Ok(src) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(&src) else {
        return Vec::new();
    };
    let Some(scenarios) = doc.get("scenarios") else {
        return Vec::new();
    };
    let Some(map) = scenarios.as_mapping() else {
        return Vec::new();
    };
    map.keys()
        .filter_map(|k| k.as_str().map(String::from))
        .collect()
}

/// Run a workload. Adapters are discovered from link-time inventory
/// registrations — the calling binary just needs to link the adapter
/// crates it wants available.
/// Execution depth: how far through the pipeline to go.
///
/// Ordering (shallowest → deepest):
/// `Phase < Dispenser < Op < Cycle < Full`.
/// `PartialOrd`/`Ord` follow this ordering so depth-gating
/// sites can write `ctx.diag.depth >= ExecDepth::Cycle` etc.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExecDepth {
    /// Compile scope-level kernels, stop before op-template
    /// kernels / adapter `map_op` / metric instruments. No
    /// adapters created.
    Phase,
    /// `dryrun=dispenser`: phase walk + every op template's
    /// dispenser is constructed (adapter `map_op` fires; the
    /// wrapper plan resolves and wraps; the cursor source
    /// factory is built). NO cycles run. This is the
    /// construction-time inspection level: catches `map_op`
    /// failures (bad prepared statement, schema mismatch),
    /// wrapper-resolver violations, source-factory init
    /// errors — without paying any per-cycle cost. Distinct
    /// from `Phase` (which builds NO dispensers) and `Op`
    /// (which runs full cycles with the wrapper short-
    /// circuit).
    Dispenser,
    /// `dryrun=op`: phase walk + dispenser construction +
    /// per-cycle wrapper-stack execution. The DRYRUN wrapper
    /// short-circuits the inner adapter `execute()`; every
    /// other layer (bind-point eval, wrapper pulls, metric
    /// timing) runs faithfully. The auto-bump in the runner
    /// lifts `Op` to `Cycle` so the cycle loop actually
    /// dispatches.
    Op,
    /// Run cycles with dry-run adapter.
    Cycle,
    /// Normal execution.
    Full,
}

/// Diagnostic configuration parsed from `dryrun=` parameter.
#[derive(Clone)]
pub struct DiagnosticConfig {
    /// How far to execute.
    pub depth: ExecDepth,
    /// Emit value-provenance / wiring view: how each named wire
    /// was computed and where its inputs originated. Surfaced by
    /// `dryrun=wiring`. (Was previously `dryrun=polydat` — the rename
    /// keeps the polydat runtime an internal concept; the user-
    /// facing concept is "wiring" between named values.)
    pub show_wiring: bool,
    /// Emit dimensional labels for all phases.
    pub show_labels: bool,
    /// Walk the post-construction component tree, render every
    /// declared dynamic control, and exit. SRD 23 §"Enumeration:
    /// controls are structural".
    pub list_controls: bool,
}

impl DiagnosticConfig {
    /// Normal execution, no diagnostics.
    pub fn normal() -> Self {
        Self {
            depth: ExecDepth::Full,
            show_wiring: false,
            show_labels: false,
            list_controls: false,
        }
    }

    /// Parse from `dryrun=` value (e.g., "phase,wiring" or "cycle").
    /// If no depth flag (phase/cycle/full) is given, defaults to `Phase`.
    pub fn parse(spec: &str) -> Self {
        let mut config = Self::normal();
        let mut depth_set = false;
        for flag in spec.split(',') {
            match flag.trim() {
                "phase" => {
                    config.depth = ExecDepth::Phase;
                    depth_set = true;
                }
                "dispenser" => {
                    config.depth = ExecDepth::Dispenser;
                    depth_set = true;
                }
                "op" => {
                    config.depth = ExecDepth::Op;
                    depth_set = true;
                }
                "cycle" => {
                    config.depth = ExecDepth::Cycle;
                    depth_set = true;
                }
                "full" => {
                    config.depth = ExecDepth::Full;
                    depth_set = true;
                }
                "wiring" => {
                    // Value-provenance view. Needs depth >= Op for
                    // kernels to exist; bump depth so a bare
                    // `dryrun=wiring` produces output instead of
                    // silently doing nothing.
                    config.show_wiring = true;
                    if !depth_set {
                        config.depth = ExecDepth::Op;
                        depth_set = true;
                    }
                }
                "labels" => config.show_labels = true,
                "controls" => {
                    // Implies an early exit before any phase
                    // runs — `controls` is a discovery dump, not
                    // an execution mode.
                    config.list_controls = true;
                    config.depth = ExecDepth::Phase;
                    depth_set = true;
                }
                // `emit` / `silent` / `json` are the dryrun output
                // modes — they live on `ActivityConfig::dry_run_mode`
                // and drive the dryrun template-parameter injection
                // that the outermost `DryRunWrapper` keys off. The
                // runner reads them via a separate
                // `params.get("dryrun")` lookup below and bumps
                // depth to Cycle so the cycle path runs. The depth-
                // config parser tolerates these tokens silently so
                // an operator passing `dryrun=fields` doesn't see a
                // misleading "unknown flag" warning.
                "fields" | "silent" | "json" => {}
                // `dryrun=kernels` is a planning-only mode — it
                // builds the scope tree, then walks every
                // materialised scope and prints its polydat
                // source. Depth stays at Phase; the runner
                // dispatches to a kernel-dump short-circuit
                // before any phase activation.
                "kernels" => {}
                _ => crate::diag!(
                    crate::observer::LogLevel::Warn,
                    "warning: unknown dryrun flag '{flag}'"
                ),
            }
        }
        // Default to phase depth if no explicit depth was given
        if !depth_set {
            config.depth = ExecDepth::Phase;
        }
        config
    }
}

/// Walk a component subtree and print every declared control
/// (name, type, current value, scope, final flag, applier
/// count) in a stable order. Used by `dryrun=controls` and any
/// other discovery-style call site.
pub fn render_controls_tree(
    root: &std::sync::Arc<std::sync::RwLock<nmbrs_metrics::component::Component>>,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    use nmbrs_metrics::component::find;
    use nmbrs_metrics::selector::Selector;

    writeln!(out, "Declared dynamic controls (SRD 23):")?;
    let all = find(root, &Selector::new());
    let mut entries: Vec<(String, String, String, String, String, String)> = Vec::new();
    for comp in all {
        let guard = match comp.read() {
            Ok(g) => g,
            Err(_) => continue,
        };
        let path = guard
            .effective_labels()
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        for ctl in guard.controls().list() {
            let scope = match ctl.branch_scope() {
                nmbrs_metrics::controls::BranchScope::Local => "local",
                nmbrs_metrics::controls::BranchScope::Subtree => "subtree",
            };
            let final_marker = match ctl.final_scope() {
                Some(s) => format!("final@{s}"),
                None => "-".to_string(),
            };
            entries.push((
                if path.is_empty() {
                    "<root>".into()
                } else {
                    path.clone()
                },
                ctl.name().to_string(),
                ctl.value_type_name().to_string(),
                ctl.value_string(),
                format!(
                    "scope={scope}, {final_marker}, appliers={}",
                    ctl.applier_count()
                ),
                if ctl.accepts_f64_writes() {
                    "f64-writable".into()
                } else {
                    "no-f64".into()
                },
            ));
        }
    }
    if entries.is_empty() {
        writeln!(out, "  (no controls declared)")?;
        return Ok(());
    }
    entries.sort();
    for (path, name, ty, value, meta, write) in entries {
        writeln!(
            out,
            "  {path}\n    {name}: {value}  [{ty}]  {meta}  {write}",
        )?;
    }
    Ok(())
}

/// Render the SRD-13d scope-elision summary for `dryrun=op`.
/// One line per scope-tree node (DFS pre-order), showing the
/// logical name and the materialised/elides-to mark.
///
/// Format follows SRD-13d §5.3:
/// ```text
/// scope elision summary
/// ------------------------
/// workload                                           materialised=true
/// workload.scenario.default                          materialised=false  elides-to=workload
/// workload.scenario.default.phase.predict            materialised=true
/// ```
///
/// `materialised=true` means the node owns a kernel; `false`
/// means it elides into its nearest materialised ancestor
/// (shown as `elides-to=<logical_name>`). Nodes whose mark
/// is still `None` (predicate hasn't fired — should not
/// happen post-`classify_and_mark`) are surfaced as `unknown`
/// rather than silently skipped.
pub fn render_scope_elision_summary(
    tree: &crate::scope_tree::ScopeTree,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    let summary = crate::scope_elision::elision_summary(tree);
    // Width of the logical-name column — 4-space gutter past
    // the longest name (or 48ch min) so the materialised marks
    // line up cleanly even with deeply-nested phase trees.
    let name_width = summary
        .iter()
        .map(|(_, _, _, name, _)| name.len())
        .max()
        .unwrap_or(0)
        .max(48);

    writeln!(out, "scope elision summary")?;
    writeln!(out, "------------------------")?;
    for (idx, _depth, materialised, logical_name, _kind) in &summary {
        match materialised {
            Some(true) => {
                writeln!(
                    out,
                    "{:<width$}    materialised=true",
                    logical_name,
                    width = name_width
                )?;
            }
            Some(false) => {
                let elides_to = tree
                    .nearest_materialised(*idx)
                    .map(|p| tree.nodes[p].logical_name.clone())
                    .unwrap_or_else(|| "<unknown>".to_string());
                writeln!(
                    out,
                    "{:<width$}    materialised=false  elides-to={}",
                    logical_name,
                    elides_to,
                    width = name_width
                )?;
            }
            None => {
                writeln!(
                    out,
                    "{:<width$}    materialised=unknown",
                    logical_name,
                    width = name_width
                )?;
            }
        }
    }
    Ok(())
}

pub async fn run(args: &[String]) -> Result<(), String> {
    // Default tui=off observer — stderr with the same Info-level
    // filter the TUI's log panel applies by default. `loglevel=`
    // CLI param overrides; absent means Info.
    //
    // We need to peek at one CLI param before kicking off the
    // full runner pipeline. Strip a leading `run` subcommand the
    // same way `run_with_observer` does at its own param-parse
    // step, so this peek doesn't reject perfectly valid CLI
    // shapes (`nmbrs run loglevel=debug …`).
    let stripped: &[String] = match args.first().map(|s| s.as_str()) {
        Some("run") => &args[1..],
        _ => args,
    };
    let cli_params = parse_params(stripped);
    let min_level = cli_params
        .get("loglevel")
        .or_else(|| cli_params.get("loglevel-display"))
        .or_else(|| cli_params.get("loglevel_display"))
        .and_then(|s| parse_log_level(s))
        .unwrap_or(crate::observer::LogLevel::Info);
    let retain_level = cli_params
        .get("loglevel-retain")
        .or_else(|| cli_params.get("loglevel_retain"))
        .and_then(|s| parse_log_level(s))
        .unwrap_or(crate::observer::LogLevel::Debug);
    crate::observer::set_retain_level(retain_level);
    crate::observer::set_display_level(min_level);
    run_with_observer(
        args,
        Arc::new(crate::observer::StderrObserver::with_min_level(min_level)),
    )
    .await
}

/// Parse a CLI/workload `loglevel=` value. Case-insensitive,
/// accepts the standard names plus the abbreviations the log
/// sink emits (`DBG` / `INF` / `WRN` / `ERR`).
pub fn parse_log_level(s: &str) -> Option<crate::observer::LogLevel> {
    use crate::observer::LogLevel;
    match s.trim().to_ascii_lowercase().as_str() {
        "trace" | "trc" => Some(LogLevel::Trace),
        "debug" | "dbg" => Some(LogLevel::Debug),
        "info" | "inf" => Some(LogLevel::Info),
        "warn" | "wrn" | "warning" => Some(LogLevel::Warn),
        "error" | "err" => Some(LogLevel::Error),
        _ => None,
    }
}

/// Run with a custom observer for phase lifecycle events.
/// The TUI persona uses this to inject a TuiObserver that updates
/// the display state instead of printing to stderr.
pub async fn run_with_observer(
    args: &[String],
    observer: Arc<dyn crate::observer::RunObserver>,
) -> Result<(), String> {
    let args: &[String] = match args.first().map(|s| s.as_str()) {
        Some("run") => &args[1..],
        // Reject unknown subcommands — don't silently fall through to execution
        Some(cmd) if !cmd.contains('=') && !cmd.ends_with(".yaml") && !cmd.ends_with(".yml") => {
            return Err(format!(
                "unknown command '{cmd}'. Use 'run' or pass a workload file."
            ));
        }
        _ => args,
    };
    // Reject conflicting duplicate `key=value` params (e.g. two
    // different `scenario=` values) before any work — otherwise the
    // last silently wins. Errors here, before session creation.
    detect_conflicting_duplicate_params(args)?;
    run_impl(args, observer).await
}

/// Core runner. Diagnostic mode is controlled by `dryrun=` param.
/// SRD-88 — build the session-level metrics services: the cadence
/// tree + reporter, the shared `MetricsQuery`, and the metrics
/// scheduler (whose `StopHandle` is returned). One set per session;
/// every execution sharing the session routes through these. Reads
/// the session component (capture root), the sqlite reporter (cadence
/// subscription), and the observer (cadence prefs + live reporters).
fn build_session_metrics(
    session: &crate::session::Session,
    sqlite_reporter: &std::sync::Arc<
        std::sync::Mutex<Option<nmbrs_metrics::reporters::sqlite::SqliteReporter>>,
    >,
    observer: &Arc<dyn crate::observer::RunObserver>,
    merged_params: &HashMap<String, String>,
    openmetrics_url: &Option<String>,
    args: &[String],
    params: &HashMap<String, String>,
) -> Result<
    (
        std::sync::Arc<nmbrs_metrics::cadence_reporter::CadenceReporter>,
        nmbrs_metrics::cadence::CadenceTree,
        std::sync::Arc<nmbrs_metrics::metrics_query::MetricsQuery>,
        std::sync::Arc<nmbrs_metrics::scheduler::StopHandle>,
    ),
    String,
> {
    // `metrics_cadence` (effective param) may set a sub-second finest
    // cadence + base interval; otherwise the default 1 s base + declared
    // cadences. The base interval drives both the cadence tree and the
    // scheduler tick below, so the finest cadence the settle detector
    // samples and the capture pulse stay in lockstep.
    let (base_interval, cadences) = resolve_cadence_config(merged_params, observer)?;
    let cadence_tree = nmbrs_metrics::cadence::CadenceTree::plan_validated(
        cadences,
        nmbrs_metrics::cadence::DEFAULT_MAX_FAN_IN,
        base_interval,
    )
    .map_err(|e| format!("cadence tree: {e}"))?;
    let cadence_reporter = Arc::new(nmbrs_metrics::cadence_reporter::CadenceReporter::new(
        cadence_tree.clone(),
    ));
    let metrics_query = Arc::new(nmbrs_metrics::metrics_query::MetricsQuery::new(
        cadence_reporter.clone(),
        session.component.clone(),
    ));
    session.set_metrics_query(metrics_query.clone());
    nmbrs_metrics::polydat_nodes::set_global_query(metrics_query.clone());
    // SRD-86 §"The metric-reader surface" / SRD-90 §M5 — install the live
    // in-process metrics-access service the `metricsql_*` nodes locate via
    // `queryapi::live_access()`. It is a HYBRID (SRD-90): the in-memory cadence
    // tier (fine, recent, retention-bounded) over the session's durable sqlite
    // store (coarse, the older tail), composed by union-minus-overlap — a recent
    // windowed read is served entirely from memory (the common case never opens
    // sqlite), and a query older than the in-memory horizon spills to the
    // durable tail. Both tiers scope to the reading execution (mem via the
    // read-exec hook; cold via `CurrentReadExec`), so a concurrent spill never
    // leaks a neighbour's series. Best-effort: if the sqlite tail can't be
    // opened (sqlite disabled, db absent), the live service is the mem tier
    // alone — byte-identical to before this seam.
    let mem_access = std::sync::Arc::new(nmbrs_metrics::queryapi::MetricsQueryAccess::new(
        metrics_query.clone(),
    ));
    // The composed read store: the in-memory cadence tier over the durable
    // sqlite tail (union-minus-overlap). The cold tier reads `All` executions
    // at the SQL level — per-execution scoping is the injected `exec_id`
    // dimensional label (below), uniform across both tiers.
    let composed: std::sync::Arc<dyn nmbrs_metrics::queryapi::MetricAccess> = {
        let db = session.output_dir.join("metrics.db");
        match nmbrs_metrics::queryapi::sqlite::SqliteDataSource::open(&db) {
            Ok(cold) => {
                let cold = cold.with_execution_selection(
                    nmbrs_metrics::queryapi::sqlite::ExecutionSelection::All,
                );
                let mem_for_horizon = mem_access.clone();
                std::sync::Arc::new(nmbrs_metrics::queryapi::HybridStore::new(vec![
                    nmbrs_metrics::queryapi::Tier::new(
                        mem_access.clone(),
                        std::sync::Arc::new(move || mem_for_horizon.earliest_ms()),
                    ),
                    nmbrs_metrics::queryapi::Tier::unbounded(std::sync::Arc::new(cold)),
                ]))
            }
            Err(e) => {
                crate::diag!(
                    crate::observer::LogLevel::Debug,
                    "metrics: hybrid sqlite tail unavailable ({e}); live reads are in-memory only"
                );
                mem_access.clone()
            }
        }
    };
    // SRD-89 §3b / SRD-90 §M6 — scope every live read to its execution via the
    // `exec_id` dimensional-label matcher, applied uniformly to both tiers.
    nmbrs_metrics::queryapi::install_live_access(std::sync::Arc::new(
        nmbrs_metrics::queryapi::ExecScopedAccess::new(composed),
    ));
    // SRD-88 — teach the live-metric reader which execution is asking, so
    // its reads scope to that execution's own series (the store is shared
    // across concurrent executions). The hook reads nmbrs-runtime's
    // task-local execution context; `None` outside any scope (single-run).
    nmbrs_metrics::queryapi::install_read_exec_id_hook(|| {
        crate::execution_context::try_current().map(|c| c.exec_id)
    });
    observer.on_metrics_query(metrics_query.clone());

    let session_for_capture = session.component.clone();
    let mut sched_builder = nmbrs_metrics::scheduler::SchedulerBuilder::new()
        .base_interval(base_interval)
        .with_cadence_reporter(cadence_reporter.clone())
        .with_cadence_tree(cadence_tree.clone());

    // SRD-42 §"SQLite — near-time persistence": subscribe the
    // SQLite reporter via the CadenceReporter push path so slow
    // disk can't stall the cascade. The subscription runs on its
    // own dispatch thread with a per-subscription timeout.
    //
    // Preferred write cadence is 30 s — coarse enough to keep
    // write volume low for long runs, fine enough for post-run
    // analysis. Aligns to the nearest declared cadence ≥ 30 s
    // (default declared set includes 30 s so this resolves exactly).
    // Journal mode is WAL (set in SqliteReporter::new via
    // `PRAGMA journal_mode=WAL`), so readers never block writers.
    //
    // Always-on: this subscription fires whenever the SQLite
    // reporter was constructed successfully. Operators don't need
    // to opt in with any extra param — every run produces a
    // `metrics.db` in its session directory by default.
    let sqlite_cadence = cadence_tree.align_to_declared(std::time::Duration::from_secs(30));
    if let (Some(cadence), Ok(guard)) = (sqlite_cadence, sqlite_reporter.lock())
        && guard.is_some()
    {
        drop(guard);
        let sqlite_for_sub = sqlite_reporter.clone();
        match cadence_reporter.subscribe(
            cadence,
            Box::new(MutexReporter(sqlite_for_sub)),
            nmbrs_metrics::cadence_reporter::SubscriptionOpts::default(),
        ) {
            Ok(_) => {
                crate::diag!(
                    crate::observer::LogLevel::Info,
                    "metrics: SQLite writes every {:?} (WAL mode)",
                    cadence
                );
            }
            Err(e) => {
                crate::diag!(
                    crate::observer::LogLevel::Warn,
                    "metrics: SQLite subscription failed: {e}"
                );
            }
        }
    }

    // Single-file metrics log — opt-in, for OUTSIDE OBSERVERS. The session
    // SQLite db is always written and stays the system of record; this only
    // duplicates the same coalesced cadence windows into one plain JSONL file so
    // a process can tail or ship metrics without linking SQLite, opening a db
    // another process is writing in WAL mode, or knowing the schema. Enable via
    // any of (a path enables it; `true` uses `<session>/metrics.jsonl`):
    //   * `--metrics-log[=<path>]` flag on the CLI
    //   * `metrics-log=<path|true>` in workload params
    //   * `NMBRS_METRICS_LOG=<path|1>` env var
    // Tell display surfaces where the session lives, so the one that keeps a
    // durable transcript can open it and flush what it buffered before the
    // directory existed. Default-on: the settled half of the display is
    // already sequenced, append-only data, so persisting it costs one file
    // handle and answers "what did the run actually show me" after the fact.
    observer.session_dir_ready(&session.output_dir);

    // Session-level system-performance sampler (host utilization from
    // /proc). Enabled per session with `sysmon=all` or a comma list of
    // categories (`sysmon=cpu,io,ram,rambw,storage`); off when absent.
    // Interval via `sysmon-interval=<seconds>` (default 5).
    //
    // rambw is REQUIRED-EXPLICIT once enabled: an unsupported host aborts
    // the session with enable instructions rather than silently monitoring
    // less than was asked for — `check_rambw_requirements` is the gate.
    if let Some(setting) = params.get("sysmon") {
        let selection = crate::sysmon::parse_selection(setting)?;
        let interval = match params.get("sysmon-interval") {
            None => std::time::Duration::from_secs(5),
            Some(v) => match v.trim_end_matches('s').parse::<f64>() {
                Ok(secs) if secs > 0.0 => std::time::Duration::from_secs_f64(secs),
                _ => {
                    return Err(format!(
                        "sysmon-interval: expected seconds (e.g. `sysmon-interval=5`), got '{v}'"
                    ));
                }
            },
        };
        let membw_peak_bytes_per_s = match params.get("sysmon-membw-gbps") {
            None => None,
            Some(v) => match v.parse::<f64>() {
                Ok(gbps) if gbps > 0.0 => Some(gbps * 1e9),
                _ => {
                    return Err(format!(
                        "sysmon-membw-gbps: expected the host's peak memory bandwidth \
                     in GB/s, got '{v}'"
                    ));
                }
            },
        };
        let mut config = crate::sysmon::SysmonConfig {
            cats: crate::sysmon::Categories::ALL,
            interval,
            membw_peak_bytes_per_s,
        };
        match selection {
            // `any` — best-effort by stated request: enable what the host
            // supports and ANNOUNCE each skip, one line per subsystem.
            crate::sysmon::Selection::Any => {
                let (cats, skipped) = crate::sysmon::resolve_any(&config);
                config.cats = cats;
                for reason in skipped {
                    crate::diag!(
                        crate::observer::LogLevel::Warn,
                        "sysmon: skipping a subsystem: {reason}"
                    );
                }
            }
            // Named categories are REQUIRED: the rambw gate runs before
            // spawn so an unsupported host is a clean session abort with
            // instructions. The session directory already exists at this
            // point (Session::start ran) — the same is true of every config
            // error raised from here, and the next run gets its own
            // directory.
            crate::sysmon::Selection::Cats(cats) => {
                config.cats = cats;
                crate::sysmon::check_rambw_requirements(&config)?;
            }
        }
        crate::sysmon::spawn(config, session.component.clone(), observer.clone())?;
        crate::diag!(
            crate::observer::LogLevel::Info,
            "sysmon: sampling {setting} every {interval:?}"
        );
    }

    let metrics_log_setting: Option<String> = args
        .iter()
        .find_map(|a| {
            a.strip_prefix("--metrics-log")
                .map(|rest| rest.strip_prefix('=').unwrap_or("true").to_string())
        })
        .or_else(|| params.get("metrics-log").cloned())
        .or_else(|| std::env::var("NMBRS_METRICS_LOG").ok());
    let metrics_log_path = match metrics_log_setting.as_deref() {
        None => None,
        Some("0") | Some("false") | Some("no") | Some("off") | Some("") => None,
        Some("1") | Some("true") | Some("yes") | Some("on") => {
            Some(session.output_dir.join("metrics.jsonl"))
        }
        Some(explicit) => Some(std::path::PathBuf::from(explicit)),
    };
    if let Some(log_path) = metrics_log_path {
        match nmbrs_metrics::reporters::metrics_log::MetricsLogReporter::new(&log_path) {
            Ok(reporter) => {
                // Same cadence as the database, deliberately: the log is an
                // alternative READ of what the db holds, so matching granularity
                // is what makes the two comparable.
                if let Some(cadence) =
                    cadence_tree.align_to_declared(std::time::Duration::from_secs(30))
                {
                    match cadence_reporter.subscribe(
                        cadence,
                        Box::new(reporter),
                        nmbrs_metrics::cadence_reporter::SubscriptionOpts::default(),
                    ) {
                        Ok(_) => {
                            crate::diag!(
                                crate::observer::LogLevel::Info,
                                "metrics: JSONL log every {:?} -> {} (session db unaffected)",
                                cadence,
                                log_path.display()
                            );
                        }
                        Err(e) => {
                            crate::diag!(
                                crate::observer::LogLevel::Warn,
                                "metrics: metrics log subscribe failed: {e}"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                crate::diag!(
                    crate::observer::LogLevel::Warn,
                    "metrics: metrics log disabled: {e}"
                );
            }
        }
    }

    // Per-instance JSONL snapshot reporter — opt-in. Writes
    // one file per (metric, label-tuple) in `<session>/metrics/`,
    // one JSON record appended per snapshot tick. Useful when
    // you want a per-instance trace to tail / awk / import
    // into a notebook without opening the SQLite db, but most
    // sessions never read these files and the SQLite db
    // already carries the same data. Enable via any of:
    //   * `--per-instance-metrics` flag on the CLI
    //   * `per-instance-metrics=true` in workload params
    //   * `NMBRS_PER_INSTANCE_METRICS=1` env var
    let per_instance_enabled = args.iter().any(|a| a == "--per-instance-metrics")
        || params
            .get("per-instance-metrics")
            .map(|s| matches!(s.as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false)
        || std::env::var("NMBRS_PER_INSTANCE_METRICS")
            .ok()
            .map(|s| matches!(s.as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);
    if per_instance_enabled {
        let per_instance_dir = session.output_dir.join("metrics");
        match nmbrs_metrics::reporters::per_instance::PerInstanceReporter::new(&per_instance_dir) {
            Ok(reporter) => {
                if let Some(cadence) =
                    cadence_tree.align_to_declared(std::time::Duration::from_secs(30))
                {
                    match cadence_reporter.subscribe(
                        cadence,
                        Box::new(reporter),
                        nmbrs_metrics::cadence_reporter::SubscriptionOpts::default(),
                    ) {
                        Ok(_) => {
                            crate::diag!(
                                crate::observer::LogLevel::Info,
                                "metrics: per-instance JSONL writes every {:?} into {}",
                                cadence,
                                per_instance_dir.display()
                            );
                        }
                        Err(e) => {
                            crate::diag!(
                                crate::observer::LogLevel::Warn,
                                "metrics: per-instance subscription failed: {e}"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                crate::diag!(
                    crate::observer::LogLevel::Warn,
                    "metrics: per-instance reporter disabled ({}): {e}",
                    per_instance_dir.display()
                );
            }
        }
    }

    // Same routing for the VictoriaMetrics / Prometheus push reporter
    // when `--report-to` (or equivalent param) was provided.
    // `jobname` / `instance` params match the nosqlbench-java
    // `PromPushReporterComponent` convention; they're substituted
    // into any `JOBNAME` / `INSTANCE` placeholders in the URL.
    if let Some(url) = openmetrics_url.as_ref()
        && let Some(cadence) = cadence_tree.align_to_declared(std::time::Duration::from_secs(10))
    {
        let jobname = merged_params
            .get("jobname")
            .cloned()
            .unwrap_or_else(|| "default".to_string());
        let instance = merged_params
            .get("instance")
            .cloned()
            .unwrap_or_else(|| "default".to_string());
        let mut vm =
            match nmbrs_metrics::reporters::victoriametrics::VictoriaMetricsReporter::from_spec(url)
            {
                Ok(r) => r,
                Err(_) => {
                    nmbrs_metrics::reporters::victoriametrics::VictoriaMetricsReporter::new(url)
                }
            };
        vm = vm.with_jobname(jobname).with_instance(instance);
        if let Some(token_path) = merged_params.get("prompush_apikeyfile") {
            match vm.with_bearer_token_file(token_path) {
                Ok(r) => vm = r,
                Err(e) => {
                    crate::diag!(
                        crate::observer::LogLevel::Warn,
                        "prompush_apikeyfile '{token_path}': {e}"
                    );
                    vm = nmbrs_metrics::reporters::victoriametrics
                            ::VictoriaMetricsReporter::from_spec(url)
                            .unwrap_or_else(|_| nmbrs_metrics::reporters::victoriametrics
                                ::VictoriaMetricsReporter::new(url))
                            .with_jobname(
                                merged_params.get("jobname").cloned()
                                    .unwrap_or_else(|| "default".to_string()),
                            )
                            .with_instance(
                                merged_params.get("instance").cloned()
                                    .unwrap_or_else(|| "default".to_string()),
                            );
                }
            }
        }
        let _ = cadence_reporter.subscribe(
            cadence,
            Box::new(vm),
            nmbrs_metrics::cadence_reporter::SubscriptionOpts::default(),
        );
    }

    // Register the observer's reporters at their requested cadences
    // on the scheduler tree (base-interval live-frame forwarding for
    // sparklines / live histogram).
    for (interval, reporter) in observer.reporters() {
        sched_builder = sched_builder.add_reporter(interval, BoxedReporter(reporter));
    }

    let scheduler = sched_builder.build(Box::new(move || {
        nmbrs_metrics::component::capture_tree(&session_for_capture, base_interval)
    }));
    let stop_handle = Arc::new(scheduler.start());

    // Install the session-wide Ctrl-C handler. First SIGINT
    // requests cooperative shutdown (fibers exit at cycle
    // boundary, profiler + cadence reporter flush in normal
    // teardown order); second SIGINT force-exits. Idempotent —
    // safe to call again on retry / reentry paths.
    crate::session_signals::install_signal_handler();

    // SRD-93 M7 — SIGQUIT inventory: log every Running component's
    // labels + instrument count, so "why is nothing moving" is
    // answerable from outside even when the run is wedged. Runs on
    // the signal-dispatch thread; read-only over the tree.
    let dump_root = session.component.clone();
    crate::session_signals::set_diag_dump_hook(Box::new(move || {
        fn walk(
            node: &std::sync::Arc<std::sync::RwLock<nmbrs_metrics::component::Component>>,
            out: &mut Vec<String>,
        ) {
            let g = node.read().unwrap_or_else(|e| e.into_inner());
            if g.state() == nmbrs_metrics::component::ComponentState::Running {
                out.push(format!(
                    "{} ({} instrument(s))",
                    g.effective_labels(),
                    g.instruments().len(),
                ));
            }
            for child in g.children() {
                walk(child, out);
            }
        }
        let mut lines = Vec::new();
        walk(&dump_root, &mut lines);
        crate::diag!(
            crate::observer::LogLevel::Info,
            "session: SIGQUIT inventory — {} running component(s)",
            lines.len()
        );
        for line in lines {
            crate::diag!(crate::observer::LogLevel::Info, "  {line}");
        }
    }));

    Ok((cadence_reporter, cadence_tree, metrics_query, stop_handle))
}

/// SRD-88 — the shared, session-tier context, created ONCE per session
/// (`SessionHost::setup`); every execution sharing the session runs
/// against it. Holds the session (dir / id / `session` component), the
/// durable sqlite store + shutdown guard, the session-aligned metrics
/// services and the session-tier profiler. Per-execution work +
/// workload load happens in `run_execution`.
struct SessionHost {
    session: crate::session::Session,
    sqlite_reporter:
        std::sync::Arc<std::sync::Mutex<Option<nmbrs_metrics::reporters::sqlite::SqliteReporter>>>,
    cadence_reporter: std::sync::Arc<nmbrs_metrics::cadence_reporter::CadenceReporter>,
    #[allow(dead_code)]
    cadence_tree: nmbrs_metrics::cadence::CadenceTree,
    #[allow(dead_code)]
    metrics_query: std::sync::Arc<nmbrs_metrics::metrics_query::MetricsQuery>,
    stop_handle: std::sync::Arc<nmbrs_metrics::scheduler::StopHandle>,
    refine_plan: Option<Arc<crate::refine_plan::RefinePlan>>,
    resume_target: Option<std::path::PathBuf>,
    refine_requested: bool,
    refine_scope: Option<String>,
    /// SRD-106 — the session id the `stick_session` rung
    /// re-attached to, when it engaged. Drives the D4
    /// `session_notice` announcement (first notable event of
    /// the run) and nothing else.
    stick_reattached: Option<String>,
    profiler: Option<crate::profiler::ProfileGuard>,
    sqlite_guard: nmbrs_metrics::reporters::sqlite::SqliteShutdownGuard,
    /// SRD-88 — the checkpoint writer is SESSION-tier: one per session
    /// (`<session>/checkpoint.jsonl` + its single resume lock), shared
    /// by every execution. Per-execution resume *plans* are still
    /// derived per execution in `run_execution` from `saved_doc` + that
    /// execution's pre-map; only the writer/lock is shared (so N
    /// concurrent in-process executions don't fight over the lock).
    checkpoint_writer: std::sync::Arc<crate::checkpoint::CheckpointWriter>,
    saved_doc: Option<crate::checkpoint::Checkpoint>,
}

impl SessionHost {
    /// Build the shared session-tier context. Workload-INDEPENDENT:
    /// session identity = `scenario=` param; metrics services +
    /// profiler read CLI `params`, not workload-merged params.
    fn setup(
        args: &[String],
        observer: Arc<dyn crate::observer::RunObserver>,
    ) -> Result<SessionHost, String> {
        // Set global observer so all code can log through it
        crate::observer::set_global_observer(observer.clone());

        // Wire error handler logging through the observer.
        // Per-cycle error lines fire from inside an executing
        // phase, so prefix with the running phase's scope-depth
        // indent — the same alignment the polling-op messages,
        // phase startup/complete lines, and DONE summary use.
        // The errorhandler crate stays scope-agnostic; the
        // bridging closure here is what makes the output
        // hierarchic in tui=terminal mode.
        //
        // Level = Debug so the per-cycle warns land in the
        // session log (retain_level defaults to Debug) but
        // don't spam the realtime status surface (display_level
        // defaults to Info). The structured form of each
        // per-cycle error is collected into the phase's
        // PhaseErrorDetail buffer and rendered in one block by
        // the `error_readout` builtin at PhaseEnd — the
        // operator sees the normative ✓/✗ phase line first,
        // then the error block, instead of N noisy lines
        // interleaved with progress as the phase runs.
        nmbrs_errorhandler::handlers::set_log_fn(|msg| {
            let indent = crate::scene_tree::running_phase_indent();
            crate::observer::log(crate::observer::LogLevel::Debug, &format!("{indent}{msg}"));
        });

        // Route nmbrs-metrics diagnostic warnings through the observer so
        // reporter write failures, histogram-record errors, etc. don't
        // slip past the TUI as raw stderr prints. Indent matches the
        // running phase the same way the errorhandler bridge above does
        // — these emits fire mid-phase from the metrics pipeline.
        nmbrs_metrics::diag::set_warn_fn(|msg| {
            let indent = crate::scene_tree::running_phase_indent();
            crate::observer::log(crate::observer::LogLevel::Warn, &format!("{indent}{msg}"));
        });
        nmbrs_metrics::diag::set_info_fn(|msg| {
            let indent = crate::scene_tree::running_phase_indent();
            crate::observer::log(crate::observer::LogLevel::Info, &format!("{indent}{msg}"));
        });

        // Audit sink is installed after session creation
        // below (so it can target `<session>/audit.log`).
        // Until then, the crate-default eprintln fallback
        // is in effect for any audit::log calls fired
        // during init. Workload-emitted lines fire mid-phase
        // (well after the install), so they don't reach
        // stderr.

        let args = normalize_args(args);
        let params = parse_params(&args);
        // The run's effective params (workload `params:` overlaid by CLI, CLI
        // wins) — the consolidated set the session-tier services read, so a
        // setting like `metrics_cadence` / `jobname` / `per-instance-metrics`
        // works whether declared in the workload or passed on the command line.
        // Operational reads below (resume, session identity) stay on the raw CLI
        // `params` — they are not workload-declarable.
        let eff_params = effective_params(&args);
        // `scenario_for_session` is recomputed below from the refine block's
        // params (the session-dir name); `openmetrics_url` feeds the
        // session metrics services.
        let openmetrics_url: Option<String> = cli_flag_value(&args[..], "--report-openmetrics-to")
            .or_else(|| {
                args.iter()
                    .find_map(|a| a.strip_prefix("report-openmetrics-to="))
                    .map(|s| s.to_string())
            });
        // SRD-106 Part 3 — the `stick_session` resolution rung: the
        // LOWEST-precedence session rung. A workload may declare that
        // iterative re-attachment is its intended usage; when the
        // operator expressed no session intent of their own, a run
        // re-attaches to `sessions/latest` and layers a new execution
        // per SRD-77 — mechanically the refine path, so the D2
        // provenance gates govern what re-runs. Defeated by ANY
        // explicit session selection (`--session*` name/path/reuse),
        // any re-attachment flag (`resume=` / `--resume-latest` /
        // `--refine`), the `--session new` bare token, and dryruns
        // (resume-inert per SRD-44). CLI `stick_session=true|false`
        // overrides the workload's declaration.
        let stick_reattach: Option<std::path::PathBuf> = {
            let cli_stick: Option<bool> =
                params.get("stick_session").map(|v| v == "true" || v == "1");
            let stick_on =
                cli_stick.unwrap_or_else(|| peek_stick_session(&params, &args).unwrap_or(false));
            let reattach_expressed = args
                .iter()
                .any(|a| a == "--refine" || a == "--resume-latest")
                || params.contains_key("resume")
                || params.contains_key("resume_latest");
            if !stick_on || reattach_expressed || crate::session::args_request_dryrun(&args) {
                None
            } else {
                let spec = crate::session::resolve_session_dir(&args);
                let operator_selected = !spec.is_empty()
                    || spec.force_new
                    || spec.reuse != crate::session::SessionReuse::Error;
                if operator_selected {
                    None
                } else {
                    // `sessions/latest` must resolve to a prior session
                    // with a checkpoint — anything less means there is
                    // nothing valid to re-attach to, and the run falls
                    // through to today's fresh-session default silently
                    // (no announcement for a no-op).
                    let latest = crate::session::default_sessions_root().join("latest");
                    std::fs::read_link(&latest)
                        .ok()
                        .map(|t| {
                            if t.is_absolute() {
                                t
                            } else {
                                crate::session::default_sessions_root().join(t)
                            }
                        })
                        .filter(|d| d.join("checkpoint.jsonl").is_file())
                }
            }
        };
        let stick_reattached: Option<String> = stick_reattach
            .as_ref()
            .and_then(|d| d.file_name())
            .and_then(|s| s.to_str())
            .map(String::from);
        if let Some(id) = stick_reattached.as_deref() {
            crate::diag!(
                crate::observer::LogLevel::Info,
                "stick_session: re-attaching to {id} — pass `--session new` to start fresh"
            );
        }

        let resume_target: Option<std::path::PathBuf> = {
            let explicit = params.get("resume").filter(|s| !s.is_empty()).map(|s| {
                let p = std::path::PathBuf::from(s);
                if p.is_file() {
                    p
                } else if p.is_dir() {
                    p.join("checkpoint.jsonl")
                } else {
                    crate::session::default_sessions_root()
                        .join(s)
                        .join("checkpoint.jsonl")
                }
            });
            let resume_latest = params
                .get("resume_latest")
                .map(|s| s != "false" && s != "0")
                .unwrap_or(false)
                || args.iter().any(|a| a == "--resume-latest")
                || stick_reattach.is_some();
            if resume_latest {
                // Resolve the symlink to a concrete session dir
                // *now* — once `Session::new` runs the symlink will
                // be repointed at the new session.
                let latest = crate::session::default_sessions_root().join("latest");
                let resolved = std::fs::read_link(&latest)
                    .ok()
                    .map(|target| {
                        if target.is_absolute() {
                            target
                        } else {
                            crate::session::default_sessions_root().join(target)
                        }
                    })
                    .map(|d| d.join("checkpoint.jsonl"));
                explicit.or(resolved)
            } else {
                explicit
            }
        };

        // SRD-77 refine: the `nmbrs refine` verb injects `--refine`
        // into argv before delegating to the runner. Detected here
        // so we can load the session's prior `phase_outcomes` and
        // build a skip plan + bump `exec_id` before the session is
        // re-attached. Implies `--resume-latest` semantics for
        // session dir resolution (the resolver at line 950+ already
        // produces the right `resume_target` when `--resume-latest`
        // was passed alongside `--refine` by `refine_command`).
        // SRD-106: an engaged `stick_session` re-attach IS a refine
        // layering — new execution, prior outcomes as provenance.
        let refine_requested = args.iter().any(|a| a == "--refine") || stick_reattach.is_some();

        // Session: root context for this run. Creates logs/{scenario}_{timestamp}/
        // for fresh runs; reuses the prior session dir when resuming
        // so the metrics.db is appended-to in-place per SRD-44
        // §"Wholesale metrics-purge".
        let scenario_for_session = params
            .get("scenario")
            .map(|s| s.as_str())
            .unwrap_or("default");
        // SRD-77 — refine_plan is populated only when:
        //   1. `--refine` was passed (refine verb is in flight)
        //   2. The resume_target resolves to an existing session dir
        // Computed BEFORE Session construction so the plan's
        // `next_exec_id` can flow into `Session::refine`.
        // SRD-77 refine scope. Default `missing` (skip phases with
        // a prior completed outcome) when `--refine` is set without
        // an explicit `scope=`. `scope=all` builds the plan for
        // exec_id bumping + session re-attach but leaves the skip
        // set empty, so every phase runs and new outcomes overwrite
        // the prior ones (the cardinal history stays — prior rows
        // keep their old exec_id, new rows land under the bumped
        // exec_id). `scope=changed` requires `phase_hash` storage
        // on PhaseOutcome (follow-up push) and is rejected here
        // with a "not yet implemented" diag so the operator isn't
        // silently dropped to `missing` semantics.
        let refine_scope: Option<&str> = params
            .get("scope")
            .map(|s| s.as_str())
            .filter(|_| refine_requested);
        let refine_plan: Option<Arc<crate::refine_plan::RefinePlan>> = if refine_requested {
            resume_target
                .as_ref()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .and_then(|prior_dir| {
                    if !prior_dir.exists() {
                        crate::diag!(
                            crate::observer::LogLevel::Warn,
                            "refine: prior session dir not found ({}); \
                         running every phase as if this were a fresh `nmbrs run`",
                            prior_dir.display()
                        );
                        return None;
                    }
                    let mut plan =
                        crate::refine_plan::RefinePlan::load_from_session_dir(&prior_dir);
                    if plan.is_none() {
                        crate::diag!(
                            crate::observer::LogLevel::Warn,
                            "refine: no readable phase_outcomes in {}; \
                         running every phase as if this were a fresh `nmbrs run`",
                            prior_dir.display()
                        );
                    }
                    if let Some(p) = plan.as_mut() {
                        p.scope = match refine_scope {
                            Some("all") => {
                                crate::diag!(
                                    crate::observer::LogLevel::Info,
                                    "refine: scope=all — every phase will run \
                                 under exec_id={}",
                                    p.next_exec_id
                                );
                                crate::refine_plan::RefineScope::All
                            }
                            Some("changed") => {
                                crate::diag!(
                                    crate::observer::LogLevel::Info,
                                    "refine: scope=changed — comparing each \
                                 phase's program hash against the prior \
                                 outcome; unchanged phases skip, changed \
                                 phases re-run under exec_id={}",
                                    p.next_exec_id
                                );
                                crate::refine_plan::RefineScope::Changed
                            }
                            _ => crate::refine_plan::RefineScope::Missing,
                        };
                    }
                    plan.map(Arc::new)
                })
        } else {
            None
        };
        // SRD-77 — `--on-removed=` policy. When refine attaches to
        // a session whose prior outcomes name phases the current
        // workload no longer declares, the default behavior is
        // ERROR (refuse to proceed) — silently keeping orphan
        // outcomes hides intent, silently dropping them loses
        // data. `--on-removed=keep` retains them (no work);
        // `--on-removed=drop` is reserved for a future push that
        // wires the deletion + interactive confirm.
        //
        // The check compares the prior `phase_name` set against
        // the current workload's `phases:` map keys. Sweep-cell
        // variants (same name, different labels) are aggregated by
        // name here — a missing name covers every prior cell of
        // it. Tighter (name+labels) granularity is a follow-up
        // when label-set comparison becomes load-bearing.
        // (`on_removed_policy` is consulted per-execution, in `run_execution`.)
        // Build the session (the shared, session-tier container). SRD-88:
        // the session carries only `session=<id>`; the execution's
        // identity (`exec_id`, `workload`) is declared one tier down, on
        // its own component (below). [[HOST:session]]
        let session = match (refine_plan.as_ref(), resume_target.as_ref()) {
            (Some(plan), Some(p)) if p.exists() => {
                let prior_dir = p
                    .parent()
                    .map(|d| d.to_path_buf())
                    .unwrap_or_else(crate::session::latest_session_dir);
                crate::diag!(
                    crate::observer::LogLevel::Info,
                    "refine: attached to session {}; \
                 prior outcomes={}, completed phases to skip={}, \
                 next exec_id={}",
                    prior_dir.display(),
                    plan.prior_outcomes_seen,
                    plan.completed.len(),
                    plan.next_exec_id
                );
                crate::session::Session::reattach(prior_dir, scenario_for_session)
            }
            (_, Some(p)) if p.exists() => {
                let prior_dir = p
                    .parent()
                    .map(|d| d.to_path_buf())
                    .unwrap_or_else(crate::session::latest_session_dir);
                crate::session::Session::reattach(prior_dir, scenario_for_session)
            }
            _ => crate::session::Session::new_with_args(scenario_for_session, &args),
        };
        let session_log_path = session.output_dir.join("session.log");
        if let Err(e) = crate::observer::set_log_file(&session_log_path) {
            crate::diag!(
                crate::observer::LogLevel::Warn,
                "warning: failed to open session log {}: {e}",
                session_log_path.display()
            );
        }

        // `--trace=<spec>` (repeatable). Collected from raw `args`
        // because parse_params is HashMap-keyed and would collapse
        // repeated flags. See trace_router for spec grammar.
        let trace_specs = collect_repeated_flag(&args, "trace");
        match crate::trace_router::init(&trace_specs, &session.output_dir) {
            Ok(0) => {} // no --trace specified, router stays empty
            Ok(n) => crate::diag!(
                crate::observer::LogLevel::Info,
                "trace router: {n} route(s) configured"
            ),
            Err(e) => crate::diag!(
                crate::observer::LogLevel::Warn,
                "trace router init failed: {e}"
            ),
        }

        crate::diag!(
            crate::observer::LogLevel::Info,
            "session: {} ({})",
            session.id,
            session.output_dir.display()
        );

        // Polydat library audit channel: route polydat's
        // `audit::log/info/warn/...` calls through this
        // process's observer so they land in `session.log`
        // alongside every other diagnostic line, with a
        // `[lib]` subsystem tag so the operator can filter them
        // out if they're noisy. Replaces the standalone
        // `<session>/audit.log` file — same content, one fewer
        // place to look.
        // SRD-82 §"Panic reporting: one full render" — the runtime's
        // fiber/op catchers render eval-panic diagnostics in full via
        // the phase error list, so the polydat hook degrades to a
        // one-line notice instead of the full body + backtrace hint.
        polydat::set_panic_reporting_downstream(true);

        polydat::audit::set_log_fn(|level, msg| {
            use polydat::audit::LogLevel as AuditLevel;
            let mapped = match level {
                AuditLevel::Trace | AuditLevel::Debug => crate::observer::LogLevel::Debug,
                AuditLevel::Info => crate::observer::LogLevel::Info,
                AuditLevel::Warn => crate::observer::LogLevel::Warn,
                AuditLevel::Error => crate::observer::LogLevel::Error,
            };
            crate::observer::log(mapped, &format!("[lib] {msg}"));
        });

        // SQLite metrics in session directory. SRD-88: creating the
        // reporter (connection + schema + the session-INVARIANT
        // `session` metadata key) is session-tier — one per session,
        // shared by every execution. The per-execution metadata
        // (workload / scenario / params / the SRD-77 `executions` row)
        // is written separately below, per execution, so N concurrent
        // executions each record their own without clobbering.
        let sqlite_path = session.metrics_path();
        let sqlite_reporter = nmbrs_metrics::reporters::sqlite::SqliteReporter::new(&sqlite_path)
            .map(|mut r| {
                r.set_metadata("session", &session.id);
                crate::diag!(
                    crate::observer::LogLevel::Info,
                    "metrics: {}",
                    sqlite_path.display()
                );
                r
            })
            .map_err(|e| {
                crate::diag!(
                    crate::observer::LogLevel::Warn,
                    "warning: SQLite metrics disabled: {e}"
                )
            })
            .ok();
        let sqlite_reporter = std::sync::Arc::new(std::sync::Mutex::new(sqlite_reporter));

        // RAII shutdown guard — runs `consolidate_wal` at session
        // end via Drop. Reliable across every Rust unwind path:
        // normal completion, error `?` propagation, first-Ctrl-C
        // → stop flag → runner unwind. The only path that skips
        // it is `std::process::exit` (second Ctrl-C force-exit),
        // which is the operator's declared "I don't want to
        // wait" escape hatch. The guard MUST live until after
        // every reporter has finished writing — bind it here at
        // the top of the run-impl block so it drops in
        // last-created / first-dropped order relative to local
        // variables; the explicit `_` binding pins its lifetime
        // to the function scope (otherwise the temporary would
        // drop immediately).
        let _sqlite_shutdown_guard =
            nmbrs_metrics::reporters::sqlite::SqliteShutdownGuard::new(sqlite_reporter.clone());

        // Periodic WAL checkpoint so concurrent read-only
        // tooling (`nmbrs report` against a live session,
        // ad-hoc `sqlite3 metrics.db` inspection, the realtime
        // metricsql preview) sees committed writes without
        // waiting for session end. SQLite's WAL holds frames
        // until either:
        //   1. `wal_autocheckpoint` (page-count threshold,
        //      default 1000 pages) fires on a writer, OR
        //   2. an explicit `PRAGMA wal_checkpoint(...)` runs.
        //
        // Under bursty workloads (a tight rampup followed by a
        // long synchronous wait — exactly the SRD-75
        // ensure_compacted shape) writers can stall under the
        // autocheckpoint threshold for many minutes, during
        // which readers see stale data. A 60-second background
        // task running `PRAGMA wal_checkpoint(PASSIVE)` bounds
        // the staleness without blocking writers.
        //
        // PASSIVE mode is the cheap variant: it merges all
        // currently-committed WAL frames into the main `.db`
        // without truncating the WAL file or pausing writers.
        // The tokio task runs for the runtime's lifetime and is
        // cancelled on shutdown; the final `consolidate_wal`
        // (TRUNCATE flavour) at session end produces the
        // archival "no -wal sidecar" form.
        {
            let reporter = sqlite_reporter.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                // First tick is immediate; skip it so the
                // post-session-start state has a chance to
                // settle before the first checkpoint fires.
                interval.tick().await;
                loop {
                    interval.tick().await;
                    if let Ok(g) = reporter.lock()
                        && let Some(r) = g.as_ref()
                    {
                        r.passive_checkpoint();
                    }
                }
            });
        }

        // SRD-88 — session-tier metrics services, configured from CLI params.
        let (cadence_reporter, cadence_tree, metrics_query, stop_handle) = build_session_metrics(
            &session,
            &sqlite_reporter,
            &observer,
            &eff_params,
            &openmetrics_url,
            &args,
            &eff_params,
        )?;
        crate::session_signals::install_signal_handler();
        let _profiler =
            crate::profiler::ProfileGuard::maybe_start(&params, Some(&session.output_dir));

        // SRD-88 — the SESSION-tier checkpoint writer (one per session;
        // holds the single resume lock) + the resume doc. Executions share
        // the writer and each derives its own resume plan from `saved_doc`.
        let checkpoint_path = session.output_dir.join("checkpoint.jsonl");
        let saved_doc = match resume_target.as_ref() {
            Some(p) => match crate::checkpoint::storage::read(p) {
                Ok(Some(doc)) => Some(doc),
                Ok(None) => {
                    crate::diag!(
                        crate::observer::LogLevel::Warn,
                        "resume: no checkpoint found at {} — fresh session",
                        p.display()
                    );
                    None
                }
                Err(e) => return Err(format!("resume: {e}")),
            },
            None => None,
        };
        let invocation = saved_doc.as_ref().map(|d| d.invocation + 1).unwrap_or(1);
        let started_at = saved_doc
            .as_ref()
            .map(|d| d.started_at.clone())
            .unwrap_or_else(crate::checkpoint::storage::now_rfc3339);
        // A dry-run is resume-inert (SRD-44): it short-circuits ops and may
        // run against placeholder params, so it must persist NO checkpoint —
        // otherwise its (synthetic) phase completions poison a later
        // `--resume-latest`. `Session::new_with_args` already withholds the
        // `latest` symlink from a dry-run; this withholds the checkpoint too.
        let checkpoint_writer =
            std::sync::Arc::new(if crate::session::args_request_dryrun(&args) {
                crate::checkpoint::CheckpointWriter::disabled(checkpoint_path.clone())
            } else {
                match saved_doc.as_ref() {
                    Some(_doc) => crate::checkpoint::CheckpointWriter::from_existing(
                        checkpoint_path.clone(),
                        saved_doc.clone().unwrap(),
                        crate::checkpoint::storage::now_rfc3339(),
                        invocation,
                    ),
                    None => crate::checkpoint::CheckpointWriter::new(
                        checkpoint_path.clone(),
                        session.id.clone(),
                        started_at,
                        invocation,
                    ),
                }
            });

        Ok(SessionHost {
            session,
            sqlite_reporter,
            cadence_reporter,
            cadence_tree,
            metrics_query,
            stop_handle,
            refine_plan,
            resume_target,
            refine_requested,
            refine_scope: refine_scope.map(|s| s.to_string()),
            stick_reattached,
            profiler: _profiler,
            sqlite_guard: _sqlite_shutdown_guard,
            checkpoint_writer,
            saved_doc,
        })
    }

    /// Session teardown — once, after every execution (SRD-88).
    async fn shutdown(self) {
        if let Some(mut profiler) = self.profiler {
            profiler.finish();
        }
        self.stop_handle.stop().await;
        let _teardown_t = std::time::Instant::now();
        self.cadence_reporter.shutdown().await;
        crate::diag!(
            crate::observer::LogLevel::Debug,
            "shutdown: cadence reporter flush+join {:?}",
            _teardown_t.elapsed()
        );
        // Release the live-access reader (the HybridStore's sqlite cold-tier
        // connection on metrics.db) BEFORE consolidating the WAL: the
        // `journal_mode=DELETE` flip in `consolidate_wal` needs an EXCLUSIVE
        // db lock, which a still-open reader connection on the same file
        // blocks ("database is locked"). The session is fully stopped here, so
        // no live reads remain.
        nmbrs_metrics::queryapi::uninstall_live_access();
        crate::diag!(
            crate::observer::LogLevel::Info,
            "shutting down — consolidating metrics.db WAL"
        );
        self.sqlite_guard.consume();
        crate::diag!(crate::observer::LogLevel::Info, "shutdown complete");
        // Shutdown-ladder bookkeeping: the run drained through the full
        // process-level cleanup, so a still-ticking level-1 countdown
        // (Ctrl-C during the final drain) goes quiet instead of
        // announcing op cancellation for a run that no longer has ops.
        crate::session_signals::mark_shutdown_complete();
    }
}

/// SRD-88 — run ONE execution against a shared [`SessionHost`]. Does
/// NOT tear the session down (that is `SessionHost::shutdown`).
async fn run_execution(
    host: &SessionHost,
    args: &[String],
    observer: Arc<dyn crate::observer::RunObserver>,
) -> Result<(), String> {
    let session = &host.session;
    let session_id = host.session.id.clone();
    let sqlite_reporter = host.sqlite_reporter.clone();
    let cadence_reporter = host.cadence_reporter.clone();
    let stop_handle = host.stop_handle.clone();
    let resume_target = host.resume_target.clone();
    let refine_plan = host.refine_plan.clone();
    let refine_requested = host.refine_requested;
    let refine_scope = host.refine_scope.as_deref();
    let stick_reattached = host.stick_reattached.clone();
    let mut diag = DiagnosticConfig::normal();

    // Detect scenario shorthand: `workload.yaml <scenario_name>` → `scenario=<name>`
    let args = normalize_args(args);
    let mut params = parse_params(&args);

    // ── SRD-109 Part 2 — driver-manifest resolution ──
    //
    // `driver=<name>` resolves a driver manifest (local
    // `drivers/<name>/driver.yaml` under the cwd first, then the
    // bundled catalog entry `drivers/<name>/driver`; both at once
    // is a hard error, mirroring workload resolution). The
    // manifest supplies the implementation library (as workload=
    // when none was given, else as impl=), the backing adapter,
    // and default params at lowest precedence. A `driver=` value
    // matching no manifest keeps its legacy meaning: an alias for
    // `adapter=`.
    if let Some(driver_name) = params.get("driver").cloned()
        && let Some((manifest, library_ref)) = resolve_driver_manifest(&driver_name)?
    {
        // A positional `<file>.yaml` argument counts as a given
        // workload — the manifest's library must not displace it.
        let workload_given = params.contains_key("workload")
            || args
                .iter()
                .any(|a| a.ends_with(".yaml") || a.ends_with(".yml"));
        apply_driver_manifest(
            &mut params,
            &driver_name,
            manifest,
            library_ref,
            workload_given,
        )?;
    }
    let params = params;

    // Load workload — from inline op= or YAML file.
    let mut workload_file: Option<String> = None;
    // SRD-85: true when the workload came from the bundled
    // catalog — its identity is a catalog name, not a path, and
    // relative resolution context falls back to the cwd.
    let mut workload_is_bundled = false;
    let mut workload_source_text: Option<String> = None;
    let mut workload = if let Some(op_str) = params.get("op") {
        if params.contains_key("workload") {
            crate::diag!(
                crate::observer::LogLevel::Warn,
                "warning: op= overrides workload="
            );
        }
        nmbrs_workload::inline::synthesize_inline_workload(op_str)
            .map_err(|e| format!("inline workload: {e}"))?
    } else {
        let workload_raw = params
            .get("workload")
            .cloned()
            .or_else(|| {
                args.iter()
                    .find(|a| a.ends_with(".yaml") || a.ends_with(".yml"))
                    .cloned()
            })
            .ok_or("no workload specified. Use workload=file.yaml or op=\"...\"")?;

        // SRD-85 resolution: local files first (as-is, with
        // extensions, under cwd `workloads/`), then the bundled
        // catalog by exact name. Both at once is a hard error.
        match resolve_workload(&workload_raw)? {
            ResolvedWorkload::Path(workload_path) => {
                workload_file = Some(workload_path.clone());

                // SRD-72: parse_workload_from_path resolves any
                // `extends:` chain before delegating to parse_workload.
                // The merged YAML text is what the parser consumes; the
                // raw on-disk text is still kept for diagnostic
                // `<path>:<line>:<col>` reporting (no source-map across
                // include boundaries today — diagnostics on inherited
                // fields point at the merged-output position).
                let yaml_source = std::fs::read_to_string(&workload_path)
                    .map_err(|e| format!("read workload '{workload_path}': {e}"))?;
                let workload = nmbrs_workload::parse::parse_workload_from_path(
                    std::path::Path::new(&workload_path),
                    &params,
                )
                .map_err(|e| format!("parse workload: {e}"))?;
                workload_source_text = Some(yaml_source);
                workload
            }
            ResolvedWorkload::Bundled(bundled) => {
                // Catalog name is the workload identity for the
                // session (session.log, summaries, phase
                // outcomes) — bundled runs have no path.
                workload_file = Some(bundled.name.to_string());
                workload_is_bundled = true;
                // SRD-72 + SRD-85: a bundled workload's
                // `extends:` chain resolves through the catalog
                // (no directory context).
                let (merged, res_warnings) =
                    nmbrs_workload::extends::load_and_merge_bundled(bundled)
                        .map_err(|e| format!("bundled workload `{}`: {e}", bundled.name))?;
                let mut workload = nmbrs_workload::parse::parse_workload(&merged, &params)
                    .map_err(|e| format!("parse bundled workload `{}`: {e}", bundled.name))?;
                workload.resolution_warnings.extend(res_warnings);
                workload_source_text = Some(bundled.source.to_string());
                workload
            }
        }
    };

    // ── SRD-108 Part B — implementation binding (load time) ──
    //
    // Two invocation forms:
    //   workload=<impl>                  — the impl's `implements:`
    //       pulls its blueprint; the BLUEPRINT becomes the
    //       effective workload with the impl's op bodies bound in.
    //   workload=<blueprint> impl=<impl> — the blueprint is the
    //       entry point; `impl=` names the implementation, whose
    //       own `implements:` must resolve to the same blueprint.
    // All rules are load errors; by the time synthesis runs the
    // workload is ordinary and fully concrete.
    let impl_param = params.get("impl").cloned();
    if let Some(target) = workload.implements.clone() {
        if impl_param.is_some() {
            return Err(format!(
                "workload '{}' is an implementation (declares \
                 `implements:`), so `impl=` does not apply — invoke \
                 either the implementation directly or the blueprint \
                 with impl=",
                workload_file.as_deref().unwrap_or("<inline>")
            ));
        }
        let base_dir = (!workload_is_bundled)
            .then(|| {
                workload_file
                    .as_deref()
                    .map(std::path::Path::new)
                    .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            })
            .flatten();
        let bundled_origin = workload_is_bundled
            .then(|| workload_file.as_deref())
            .flatten();
        let (mut blueprint, blueprint_id) =
            load_secondary_workload(&target, &params, base_dir.as_deref(), bundled_origin)?;
        crate::diag!(
            crate::observer::LogLevel::Info,
            "implements: binding '{}' into blueprint '{blueprint_id}'",
            workload_file.as_deref().unwrap_or("<inline>")
        );
        nmbrs_workload::implements::bind_implementation(&mut blueprint, workload)
            .map_err(|e| format!("implements binding: {e}"))?;
        workload = blueprint;
    } else if let Some(impl_ref) = impl_param {
        let base_dir = (!workload_is_bundled)
            .then(|| {
                workload_file
                    .as_deref()
                    .map(std::path::Path::new)
                    .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            })
            .flatten();
        let bundled_origin = workload_is_bundled
            .then(|| workload_file.as_deref())
            .flatten();
        let (implementation, impl_id) =
            load_secondary_workload(&impl_ref, &params, base_dir.as_deref(), bundled_origin)?;
        let declared = implementation.implements.clone().ok_or_else(|| {
            format!(
                "impl='{impl_ref}' resolves to '{impl_id}', which declares no \
             `implements:` — an implementation module must name its \
             blueprint"
            )
        })?;
        // The impl's declared target must be the SAME document the
        // operator invoked as workload=.
        // The impl doc's `implements:` resolves relative to the
        // IMPL doc's own directory (extends precedent).
        let impl_dir = std::path::Path::new(&impl_id)
            .parent()
            .map(|d| d.to_path_buf());
        let declared_id =
            workload_ref_identity(&declared, impl_dir.as_deref(), Some(impl_id.as_str()))?;
        let invoked_id = workload_file
            .clone()
            .map(|f| canonical_identity(&f))
            .unwrap_or_default();
        if declared_id != invoked_id {
            return Err(format!(
                "impl='{impl_id}' declares implements='{declared}' \
                 (resolves to '{declared_id}'), which is not the invoked \
                 workload '{invoked_id}'"
            ));
        }
        crate::diag!(
            crate::observer::LogLevel::Info,
            "implements: binding '{impl_id}' into blueprint '{invoked_id}'"
        );
        nmbrs_workload::implements::bind_implementation(&mut workload, implementation)
            .map_err(|e| format!("implements binding: {e}"))?;
    }
    {
        let unbound = nmbrs_workload::implements::unbound_abstract_slots(&workload);
        if !unbound.is_empty() {
            return Err(format!(
                "abstract op slot(s) [{}] unbound — pass impl=<workload> \
                 or invoke an implementing workload (SRD-108)",
                unbound.join(", ")
            ));
        }
    }

    // Overlay CLI params on the workload's declared params (CLI wins) so
    // synthesis (next) can read the operator's `cycles=N` / `concurrency=N`
    // overrides and promote them onto the synthetic phase. Same precedence
    // rule the session-tier services use via [`effective_params`], so a key
    // means the same thing whether declared in the workload or on the CLI.
    // Also covers the inline (`op=`) path that loaded empty params.
    workload.params = overlay_cli_params(std::mem::take(&mut workload.params), &params);

    // Unification: the scenario-tree executor is the sole
    // execution path. Workloads that arrive without an
    // explicit `phases:` block (the `op=` inline form, the
    // `blocks:` shorthand, top-level `ops:` lists) get an
    // implicit `main` phase + `default` scenario synthesized
    // here. Idempotent on workloads that already declare
    // phases. See [`nmbrs_workload::model::Workload::synthesize_default_phase`].
    workload.synthesize_default_phase();

    let merged_params = workload.params.clone();

    // Extract core config
    let driver = merged_params
        .get("adapter")
        .or_else(|| merged_params.get("driver"))
        .cloned()
        .unwrap_or_else(|| "stdout".into());
    let explicit_cycles: Option<u64> = merged_params.get("cycles").and_then(|s| parse_count(s));
    let concurrency: usize = match merged_params.get("concurrency") {
        Some(s) => s
            .parse()
            .map_err(|_| format!("concurrency value '{s}' is not a valid integer"))?,
        None => 1,
    };
    let rate: Option<f64> = match merged_params.get("rate") {
        Some(s) => Some(
            s.parse()
                .map_err(|_| format!("rate value '{s}' is not a valid number"))?,
        ),
        None => None,
    };
    // Workload-root total-attempts budget — the `tries` sigil for the
    // conditional TriesDispenser (SRD-82 Part 3b). Absent = ops without
    // their own `tries:` run single-attempt with no retry wrapper. `N ≥ 2`
    // retries adapter-retryable op errors (CQL timeouts/overloads) up to N
    // total attempts before the failure propagates to the result level;
    // `1` = explicit single-attempt; `0` = ops fail without executing.
    let tries: Option<u32> = match merged_params.get("tries") {
        Some(s) => Some(
            s.parse()
                .map_err(|_| format!("tries value '{s}' is not a valid integer"))?,
        ),
        None => None,
    };
    let tag_filter = merged_params.get("tags").cloned();
    let seq_type = merged_params
        .get("seq")
        .map(|s| SequencerType::parse(s).unwrap_or(SequencerType::Bucket))
        .unwrap_or(SequencerType::Bucket);
    let mut error_spec = merged_params
        .get("errors")
        .cloned()
        .unwrap_or_else(|| ".*:warn,stop".to_string());
    // `error_rate_max` — the OPT-IN session-wide error-rate circuit
    // breaker. When set, a phase fails once >this share of its ops error
    // (after a 50-op floor); per-phase `error_rate_max:` overrides it.
    // NO default (SRD-82 §"AggregateGuard retired as a default"): the
    // former silent 0.1 default was hidden, non-optional, and possibly
    // not what the operator wanted — operators built duplicate
    // `stop_when` backstops precisely because this one was invisible.
    // Aggregate health belongs to visible, workload-authored `stop_when`
    // conditions (SRD-83); this knob remains only as an explicit opt-in.
    let error_rate_max: Option<f64> = match merged_params.get("error_rate_max") {
        Some(s) => match s.trim().parse::<f64>() {
            Ok(v) if v >= 0.0 => Some(v),
            _ => {
                eprintln!(
                    "error: error_rate_max must be a non-negative number \
                           (e.g. 0.1 = 10%); got '{s}'"
                );
                std::process::exit(2);
            }
        },
        None => None,
    };
    // SRD-44 §"--force-retry-failed": when set on a resume
    // invocation, prepend a `.*:retry,warn` rule to the errors
    // cascade so any failure surfaces a retry rather than the
    // workload's normal stop / fail behaviour. Idempotent: when
    // set on a fresh run, it still applies (doesn't gate on
    // is_resume) — operators who want the override on a fresh
    // run get it; operators who pass it accidentally without
    // resume= get the same generous-retry policy they'd see on
    // resume.
    let force_retry_failed = params
        .get("force_retry_failed")
        .map(|s| s != "false" && s != "0")
        .unwrap_or(false)
        || args.iter().any(|a| a == "--force-retry-failed");
    if force_retry_failed {
        error_spec = format!(".*:retry,warn;{error_spec}");
        crate::diag!(
            crate::observer::LogLevel::Info,
            "--force-retry-failed: errors cascade prefixed with '.*:retry,warn'"
        );
    }

    // Validate CLI parameters (runner-known + adapter-registered + workload-declared).
    //
    // Allow-list = installed param vocabulary ∪ adapter-registered ∪
    // `workload.declared_params` (the original YAML keys from
    // the workload's `params:` block). We do **not** consult
    // `workload.params` here — `parse.rs` merges every CLI arg
    // into that map regardless of whether the workload declared
    // it, so checking against it would let any CLI param through
    // and silently drop typos like `profile=perf` (vs.
    // `profiler=perf`). `declared_params` preserves the user's
    // declared surface independent of CLI overlays, which is
    // what the closed-vocabulary check needs. Skipped entirely
    // when no CLI vocabulary is installed (library/test driver).
    if let Some(cli_params) = known_params() {
        let adapter_params = registered_adapter_params();
        let all_known: Vec<&str> = cli_params
            .iter()
            .copied()
            .chain(adapter_params.iter().copied())
            .chain(workload.declared_params.iter().map(|s| s.as_str()))
            .collect();
        for key in params.keys() {
            if !all_known.contains(&key.as_str()) {
                let suggestion = closest_match(key, &all_known);
                if let Some(closest) = suggestion {
                    return Err(format!(
                        "unrecognized parameter '{key}='. Did you mean '{closest}='?"
                    ));
                } else {
                    return Err(format!("unrecognized parameter '{key}='"));
                }
            }
        }
    }

    // Validate workload-declared params are actually referenced.
    // Unreferenced params can shadow runner params (e.g., a workload
    // declaring `concurrency` as a param masks the CLI parameter
    // validation, but if nothing in the workload uses `{concurrency}`
    // the value is silently ignored).
    //
    // AND the reverse direction (SRD-N param-reference validator):
    // every `{name}` placeholder in the workload must resolve to a
    // declared param, a known runner/adapter param, or an iter-var
    // introduced by some Comprehension in the scenario tree. A
    // stray `{undeclared}` would otherwise survive
    // `expand_workload_params` as a literal and trip the Polydat parser
    // later with a cryptic "expected expression, got LBrace" — that
    // surfaces too late and doesn't name the offender. The check
    // here points at the placeholder by name so the operator sees
    // what to fix.
    {
        // First — catch `{name}` placeholders that appear inside
        // Polydat expression bodies outside of string literals. The
        // Polydat grammar doesn't accept `{...}` as expression syntax;
        // a `{name}` there will always fail compile with a
        // cryptic "expected expression, got LBrace". Catch it
        // here with a targeted message naming the YAML file and
        // line so the operator can jump straight to it.
        let mut invalid_polydat_braces: Vec<PolydatBraceFinding> =
            collect_polydat_brace_refs(&workload);
        if !invalid_polydat_braces.is_empty() {
            invalid_polydat_braces.sort();
            invalid_polydat_braces.dedup();
            let file_path = workload_file.as_deref().unwrap_or("<inline>");
            let lines: Vec<String> = invalid_polydat_braces
                .iter()
                .map(|f| {
                    let yaml_line = workload_source_text
                        .as_deref()
                        .and_then(|src| find_yaml_line_for_brace(src, &f.placeholder));
                    let prefix = match yaml_line {
                        Some(n) => format!("{file_path}:{n}"),
                        None => file_path.to_string(),
                    };
                    format!(
                        "  {prefix}: in {} — `{{{}}}`. Use bare `{}`.",
                        f.location, f.placeholder, f.placeholder
                    )
                })
                .collect();
            return Err(format!(
                "`{{...}}` braces in Polydat expression context (invalid syntax).\n\
                 Polydat accepts bare identifiers; braces are only for YAML string\n\
                 interpolation (op `prepared:`/`raw:`, `cycles:`, etc.).\n{}",
                lines.join("\n"),
            ));
        }

        let referenced = collect_param_references(&workload);
        let adapter_params: std::collections::HashSet<&'static str> =
            registered_adapter_params().into_iter().collect();
        let iter_var_names = collect_iter_var_names(&workload);
        let wire_names = collect_polydat_binding_names(&workload);

        // Declared/known direction: every declared param must be
        // referenced — except adapter-registered params, which
        // the driver consumes directly from the merged params
        // (e.g. `host`/`port`/`consistency` for CQL). Declaring
        // one in `params:` is how a workload surfaces the knob
        // with a default in `describe workloads` without a
        // textual `{name}` reference.
        for name in &workload.declared_params {
            if is_cli_param(name) || adapter_params.contains(name.as_str()) {
                continue;
            }
            if !referenced.contains(name) {
                return Err(format!(
                    "workload declares param '{name}' but it is never referenced as '{{{}}}' \
                     in any op, phase, or binding. Remove it or use it.",
                    name
                ));
            }
        }

        // Undeclared direction: every curly-brace placeholder
        // must resolve. The legitimate-name set spans every
        // declaration site the runtime can satisfy:
        //   - workload.declared_params (the `params:` block)
        //   - the installed CLI param vocabulary (`cycles`, `concurrency`, …)
        //   - adapter-registered params (driver-specific config)
        //   - iter-vars from Comprehensions in the scenario tree
        //     (`k`, `limit`, `profile` from `for_each: "k in …"`)
        let declared_set: std::collections::HashSet<&str> = workload
            .declared_params
            .iter()
            .map(|s| s.as_str())
            .collect();
        let mut undeclared: Vec<&str> = referenced
            .placeholders
            .iter()
            .map(|s| s.as_str())
            .filter(|name| !declared_set.contains(*name))
            .filter(|name| !is_cli_param(name))
            .filter(|name| !adapter_params.contains(name))
            .filter(|name| !iter_var_names.contains(*name))
            .filter(|name| !wire_names.contains(*name))
            .collect();
        if !undeclared.is_empty() {
            undeclared.sort();
            return Err(format!(
                "workload references undeclared placeholder{plural} {names} — \
                 add to the `params:` block, or check for a typo. Recognised \
                 sources for `{{name}}` placeholders: workload `params:`, \
                 runner/adapter built-ins, scenario-tree iter-vars from \
                 `for_each:`/`for_combinations:`, and wire names from Polydat \
                 `bindings:`.",
                plural = if undeclared.len() == 1 { "" } else { "s" },
                names = undeclared
                    .iter()
                    .map(|n| format!("`{{{n}}}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
            ));
        }
    }

    // Extract workload structure before consuming. M3.6:
    // `workload_params` is the set of *workload-declared* params
    // (the YAML `params:` block, with CLI overrides applied) —
    // these are what get injected as `const` bindings on the
    // workload kernel. The full `workload.params` map also
    // contains ad-hoc CLI params like `cycles=`, `workload=`,
    // `tags=`, etc., which are not declared bindings and must
    // not become identifiers in the Polydat source. Filter by
    // `declared_params` to keep only the YAML-declared subset.
    let declared: std::collections::HashSet<&String> = workload.declared_params.iter().collect();
    let workload_params: HashMap<String, String> = workload
        .params
        .iter()
        .filter(|(k, _)| declared.contains(*k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    drop(declared);
    let mut phases = workload.phases;
    // Inline-expression rewrite per phase. The
    // `rewrite_inline_exprs` call later in this function (around
    // line 818) operates on `all_ops_for_compile` — a flattened
    // copy used for the workload-level kernel — but
    // `build_op_template_scope_kernel` (SRD-13d Phase 9) reads
    // op definitions from `phases.get(name).ops`, which is the
    // ORIGINAL parsed structure. Without this per-phase rewrite,
    // op-template kernels never see the synthesised
    // `__expr_N := <expr>` bindings and the conditional /
    // inline-expression machinery breaks for any op that lands
    // on the Phase 9 path. Rewriting in place here keeps the
    // two compile paths consistent.
    for phase in phases.values_mut() {
        crate::scope::rewrite_inline_exprs(&mut phase.ops);
    }
    let phase_order = workload.phase_order;
    let scenarios = workload.scenarios;
    let workload_readouts = workload.readouts.clone();
    // SRD-32a Push 3 — workload-root wrapper override.
    // Innermost-to-outermost list, extracted once and
    // installed onto every Activity via `set_wrappers_override`
    // before `run_with_driver` runs the cascade. Per-op
    // `wrappers:` overrides on individual templates shadow
    // this entry; CLI flags (not yet implemented) would set
    // it independently on the Activity.
    let workload_wrappers_override: Option<Vec<String>> = workload
        .wrappers
        .as_ref()
        .filter(|c| !c.order.is_empty())
        .map(|c| c.order.clone());
    // SRD-63 §8 / Push 8: extract the CLI `--readout=<body>`
    // override before any binder is built. Resolved through
    // the same `resolve_flag` helper as `--session-path`,
    // so it picks up the matching `NMBRS_READOUT` env var
    // when set. `None` ⇒ workload bindings + builtin
    // defaults run unchanged.
    let cli_readout_override = crate::session::resolve_flag(&args[..], "--readout");

    // SRD-32a Push 3 — CLI overrides for wrapper composition
    // ordering. Two flags:
    //
    // - `--wrap-order=<list>` — innermost-to-outermost
    //   permutation that applies to every op in this run.
    //   Workload-root and per-op blocks shadow it (config-
    //   locality wins, SRD-04 Rule 5). When neither workload-
    //   level override is set, this CLI value plumbs through
    //   to `Activity::wrappers_override` for every phase.
    // - `--wrap-default-order=<list>` — replaces the
    //   resolver's *built-in* default-order tiebreaker for
    //   the run. Useful when the operator wants a permanent
    //   tilt (e.g. always put validate outside throttle in
    //   their environment) without editing every workload.
    //   Validated against the constraint graph at session
    //   start; an inconsistent list is a hard error.
    //
    // Both flags accept a comma-separated list. Empty / unset
    // ⇒ runtime default applies.
    let cli_wrap_order: Option<Vec<String>> =
        crate::session::resolve_flag(&args[..], "--wrap-order")
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .filter(|v: &Vec<String>| !v.is_empty());
    let cli_wrap_default_order: Option<Vec<String>> =
        crate::session::resolve_flag(&args[..], "--wrap-default-order")
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .filter(|v: &Vec<String>| !v.is_empty());

    // Effective workload-level wrapper override: workload's
    // own `wrappers: { order: [...] }` block wins over the
    // CLI flag (config-locality, SRD-04 Rule 5). Per-op
    // overrides on individual ParsedOps shadow either.
    let workload_wrappers_override: Option<Vec<String>> =
        workload_wrappers_override.or(cli_wrap_order);
    // Unified report block (SRD-46). Tables auto-render at
    // end-of-run; plot specs persist into the session db so
    // post-hoc `nmbrs report ...` can replay them. Empty
    // `report:` block ⇒ no auto-render and no persisted specs.
    let workload_report = workload.report.clone();
    let workload_summaries: HashMap<String, nmbrs_workload::model::SummaryConfig> = workload_report
        .items()
        .filter(|i| matches!(i.kind, nmbrs_workload::report::Kind::Table))
        .map(|i| {
            (
                i.name.clone(),
                nmbrs_workload::model::SummaryConfig::parse(&i.body),
            )
        })
        .collect();

    // Collect ALL ops: top-level ops + all phase inline ops.
    let mut ops = workload.ops;

    // Filter top-level ops by tags (CLI-level tag filter)
    if let Some(ref filter) = tag_filter {
        ops =
            TagFilter::filter_ops(&ops, filter).map_err(|e| format!("invalid tag filter: {e}"))?;
    }

    // Classify phase ops for compilation:
    // - Phases with own bindings or for_each: saved raw, compiled per-phase
    // - Phases without own bindings: included in outer (workload) kernel
    let mut phase_ops_for_compile: Vec<nmbrs_workload::model::ParsedOp> = Vec::new();
    let mut phase_raw_ops: HashMap<String, Vec<nmbrs_workload::model::ParsedOp>> = HashMap::new();
    let mut phases_needing_own_kernel: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for (name, phase) in &phases {
        let has_own_bindings = phase.ops.iter().any(|op| !op.bindings.is_empty());
        if phase.for_each.is_some() || has_own_bindings {
            phase_raw_ops.insert(name.clone(), phase.ops.clone());
            phases_needing_own_kernel.insert(name.clone());
        } else {
            phase_ops_for_compile.extend(phase.ops.iter().cloned());
        }
    }

    // For non-phased workloads, require at least some ops
    if ops.is_empty() && phases.is_empty() {
        return Err("no ops selected (tag filter may have excluded all ops)".into());
    }

    if phases.is_empty() {
        crate::diag!(
            crate::observer::LogLevel::Info,
            "{} ops, {} cycles, concurrency={}, adapter={}",
            ops.len(),
            explicit_cycles
                .map(|c| c.to_string())
                .unwrap_or("auto".into()),
            concurrency,
            driver
        );
    } else {
        // Always log to session.log; stderr suppression is the
        // observer's job (TuiObserver gates eprintln internally).
        crate::diag!(
            crate::observer::LogLevel::Info,
            "{} phases, {} top-level ops, adapter={}",
            phases.len(),
            ops.len(),
            driver
        );
    }

    // Collect --polydat-lib=path flags
    let polydat_lib_paths: Vec<std::path::PathBuf> = args
        .iter()
        .filter_map(|a| a.strip_prefix("--polydat-lib="))
        .map(std::path::PathBuf::from)
        .collect();
    let strict = args.iter().any(|a| a == "--strict")
        || matches!(
            params.get("strict").map(String::as_str),
            Some("true") | Some("1")
        );

    // SRD-68 follow-up — session-wide op-template synthesis opt level.
    // `--kernel-opt=release|diagnostic` or `kernel_opt=…`. Release
    // (default) lets the closure-binding economy DCE unreferenced
    // magic-extern slots; Diagnostic force-allocates body/count/ok and
    // every result-binding LHS slot so step-debug / cycle-replay can
    // inspect writes the workload doesn't otherwise consume.
    let kernel_opt: polydat::kernel::KernelOptLevel = {
        let raw =
            cli_flag_value(&args[..], "--kernel-opt").or_else(|| params.get("kernel_opt").cloned());
        match raw {
            None => polydat::kernel::KernelOptLevel::default(),
            Some(s) => polydat::kernel::KernelOptLevel::parse(s.trim()).map_err(|bad| {
                format!("unknown --kernel-opt value '{bad}' — use 'release' or 'diagnostic'")
            })?,
        }
    };

    // SRD-105 — session-wide engine mix: `jit=off|auto|force` (or
    // `--jit=`). polydat 0.3 made the JIT mode a property of each
    // kernel built, never of the process, and its interpreter entry
    // points (the `PolydatKernel` path every nmbrs scope compiles
    // through) fix the mode at `Auto`; no public entry point takes a
    // mode for a program with traversals. Until polydat carries the
    // mode on `CompileOptions`, `auto` is the only honourable value:
    // `off` and `force` are refused loudly rather than accepted and
    // ignored (the differential battery is parked on the same gap).
    {
        let raw = cli_flag_value(&args[..], "--jit").or_else(|| params.get("jit").cloned());
        if let Some(s) = raw {
            match s.trim() {
                "auto" => {}
                mode @ ("off" | "force") => {
                    return Err(format!(
                        "jit={mode} is not available: polydat 0.3 fixes the interpreter \
                         kernel's JIT mode at 'auto' and exposes no per-compile override \
                         for programs with traversals; use jit=auto (the default) or \
                         omit the parameter"
                    ));
                }
                bad => {
                    return Err(format!(
                        "unknown jit value '{bad}' — use 'off', 'auto', or 'force'"
                    ));
                }
            }
        }
    }

    // Parse dryrun= param into diagnostic config
    if let Some(spec) = params.get("dryrun") {
        diag = DiagnosticConfig::parse(spec);
    }

    // skipped_phases= — how fully-gated-off phases are represented
    // (elide | mark | prune). Default: mark.
    if let Some(spec) = params.get("skipped_phases") {
        match crate::observer::SkippedPhaseDisplay::parse(spec) {
            Some(mode) => crate::observer::set_skipped_phase_display(mode),
            None => {
                return Err(format!(
                    "unknown skipped_phases value '{spec}' — use 'elide', 'mark', or 'prune'"
                ));
            }
        }
    }

    // completed_phases= — how much of a completed node's block is
    // retained in scrollback (full | headers). Default: full (SRD-92
    // R5 — completion is never a full collapse).
    if let Some(spec) = params.get("completed_phases") {
        match crate::observer::CompletedPhaseDisplay::parse(spec) {
            Some(mode) => crate::observer::set_completed_phase_display(mode),
            None => {
                return Err(format!(
                    "unknown completed_phases value '{spec}' — use 'full' or 'headers'"
                ));
            }
        }
    }

    // "Never Ignore Silently" — scenario-parse errors are
    // ALWAYS fatal regardless of strict mode. The parser used
    // to silently drop unknown scenario-node keys (e.g.
    // `iterate:` misspellings, stray `phases:` siblings),
    // which led to downstream "phase 'iterate' not found"
    // confusion masking the real bug. The parser now collects
    // every malformed-node case here and we refuse to dispatch.
    if !workload.scenario_parse_errors.is_empty() {
        return Err(format!(
            "scenario parse error{plural} — workload is malformed:\n  - {msgs}",
            plural = if workload.scenario_parse_errors.len() == 1 {
                ""
            } else {
                "s"
            },
            msgs = workload.scenario_parse_errors.join("\n  - "),
        ));
    }

    // SRD-46 + SRD-15: surface report-block warnings collected
    // by the parser. Strict mode promotes to a hard error so a
    // workload with `defaults`-collisions or empty groups can't
    // silently pass; otherwise we log them and continue.
    if !workload.report_warnings.is_empty() {
        if strict {
            return Err(format!(
                "report-block warnings (strict mode promotes to errors):\n  - {}",
                workload.report_warnings.join("\n  - "),
            ));
        }
        for w in &workload.report_warnings {
            crate::observer::log_tagged(
                crate::observer::LogLevel::Warn,
                crate::observer::EventTag::in_flight(crate::observer::EventCategory::Report),
                &format!("report: {w}"),
            );
        }
    }

    // SRD-85 nearest-first reference resolution: a target that
    // matched multiple same-named resources resolved to the
    // nearest (filesystem favored) and is surfaced here — the
    // shadowing is allowed but never silent. Strict promotes.
    if !workload.resolution_warnings.is_empty() {
        if strict {
            return Err(format!(
                "reference-resolution warnings (strict mode promotes to errors):\n  - {}",
                workload.resolution_warnings.join("\n  - "),
            ));
        }
        for w in &workload.resolution_warnings {
            crate::observer::log_tagged(
                crate::observer::LogLevel::Warn,
                crate::observer::EventTag::in_flight(crate::observer::EventCategory::Resolution),
                &format!("resolve: {w}"),
            );
        }
    }

    // Dry-run mode resolution.
    //
    // The mode string is a LABEL — there is no "silent" or
    // "op" or "cycle" adapter. The real adapter from the
    // workload is constructed in full (connect, prepare,
    // metadata, dispenser init); the `DryRunWrapper` is
    // installed at the outermost wrapper position and per-
    // cycle short-circuits the inner stack so the adapter's
    // `execute()` never fires. The mode string carries
    // operator intent through the log (`dryrun: injected
    // `dryrun: <mode>` …`) and only the `fields` mode has a
    // structural side-effect (forces the fields wrapper on so
    // rendered op text reaches stdout).
    //
    // Mapping:
    //   dryrun=cycle  → mode="cycle",  wrapper short-circuit
    //   dryrun=op     → mode="op",     wrapper short-circuit
    //   dryrun=silent → mode="silent", wrapper short-circuit
    //   dryrun=fields → mode="fields", wrapper short-circuit + fields-render on
    //   dryrun=full   → mode=None,     real execution
    let dry_run: Option<&str> = if diag.depth == ExecDepth::Cycle {
        Some("cycle")
    } else {
        params.get("dryrun").and_then(|s| match s.as_str() {
            "fields" => Some("fields"),
            "silent" => Some("silent"),
            "op" => Some("op"),
            _ => None,
        })
    };

    // Auto-bump depth to Cycle for any dryrun mode that
    // installs the wrapper — cycles must dispatch for the
    // wrapper to have anything to short-circuit. Without the
    // bump, `dryrun=silent` / `dryrun=fields` / `dryrun=op`
    // would silently produce no output because the
    // phase-early-complete branch at executor.rs:2998 elides
    // the cycle loop when depth < Cycle.
    if dry_run.is_some() && diag.depth < ExecDepth::Cycle {
        diag.depth = ExecDepth::Cycle;
    }

    // (OpenMetrics push URL is resolved in `SessionHost::setup` — the
    // metrics push reporter is a session-tier service.)

    // Resolve the resume source BEFORE creating the new session
    // — `Session::new` eagerly remaps `logs/latest` at the new
    // session id, so any path resolution that depends on the old
    // `latest` target has to happen first. Stored as
    // `resume_target` and consulted later when constructing the
    // checkpoint writer + plan. SRD-44 §"Resume CLI surface".
    //
    // SRD-88 — per-execution: the session name (host already used the
    // same value to name the session dir) + the refine on-removed
    // policy, recomputed here from this execution's params.
    let scenario_for_session = params
        .get("scenario")
        .map(|s| s.as_str())
        .unwrap_or("default");
    let on_removed_policy: &str = params
        .get("on_removed")
        .map(|s| s.as_str())
        .unwrap_or("error");
    if let Some(plan) = refine_plan.as_ref() {
        let current_names: std::collections::HashSet<&str> =
            phases.keys().map(|s| s.as_str()).collect();
        let mut removed: Vec<&str> = plan
            .seen_identities
            .iter()
            .map(|(name, _)| name.as_str())
            .filter(|n| !current_names.contains(n))
            .collect();
        removed.sort();
        removed.dedup();
        if !removed.is_empty() {
            match on_removed_policy {
                "error" => {
                    return Err(format!(
                        "refine: workload removes {n} phase{plural} that have \
                         prior outcomes in this session:\n  - {names}\n\
                         Pass `on_removed=keep` to retain the prior outcomes \
                         (no work, no error); `on_removed=drop` is reserved \
                         (not yet implemented). Default `error` refuses to \
                         proceed so accidental axis-trim doesn't drop history \
                         silently.",
                        n = removed.len(),
                        plural = if removed.len() == 1 { "" } else { "s" },
                        names = removed.join("\n  - "),
                    ));
                }
                "keep" => {
                    crate::diag!(
                        crate::observer::LogLevel::Info,
                        "refine: on_removed=keep — retaining prior outcomes \
                         for {n} removed phase(s): {names}",
                        n = removed.len(),
                        names = removed.join(", ")
                    );
                }
                "drop" => {
                    crate::diag!(
                        crate::observer::LogLevel::Warn,
                        "refine: on_removed=drop is reserved — not yet \
                         implemented. Treating as `keep` (retaining prior \
                         outcomes) for now: {names}",
                        names = removed.join(", ")
                    );
                }
                other => {
                    return Err(format!(
                        "refine: unknown on_removed= policy '{other}'; \
                         expected `error` (default), `keep`, or `drop`"
                    ));
                }
            }
        }
    }

    // SRD-88 — a CONCURRENT execution runs inside a scoped
    // `ExecutionContext` whose `exec_id` was allocated distinctly per
    // sibling; use it so each concurrent execution's metric rows /
    // metadata / `executions` row are separable. Outside a scoped
    // context (single-run), fall back to the SRD-77 verb/exec_id:
    // `refine` numbers from the prior outcomes' max+1, `run`/`resume`
    // start at 1 — byte-identical to before.
    let (exec_verb, exec_id_seed): (&'static str, u64) =
        match crate::execution_context::try_current() {
            Some(ctx) => ("run", ctx.exec_id),
            None => match (refine_plan.as_ref(), resume_target.as_ref()) {
                (Some(plan), Some(p)) if p.exists() => ("refine", plan.next_exec_id),
                (_, Some(p)) if p.exists() => ("resume", 1),
                _ => ("run", 1),
            },
        };
    // SRD-88 §2 — start this execution under the session: derive
    // its component (carrying `exec_id` + `workload`) as a child
    // of the session component. Phase components attach under
    // `execution.component`, so every metric inherits this
    // execution's identity without any tier redeclaring a label.
    let execution = crate::session::Execution::start(
        &session,
        workload_file.as_deref().unwrap_or("inline"),
        scenario_for_session,
        exec_verb,
        exec_id_seed,
    );
    let exec_id = execution.exec_id;

    // dryrun=controls: defer the tree walk until after phase
    // construction. `list_controls` implies depth=Phase, which
    // means every phase compiles and attaches its component —
    // that's when activity-scoped controls get declared — but
    // no cycles run. Walking here would only see session-root
    // controls. The renderer fires at the very end of the run,
    // just before the session returns.

    // Direct the diagnostic log sink at <session_dir>/session.log so every
    // observer::log() call is captured durably, even under the TUI.
    // SRD-77 / SRD-88 — per-execution metadata + the in-flight
    // `executions` row. Written through the shared session reporter
    // but scoped to THIS execution's `exec_id`, so concurrent
    // executions sharing the session each record their own workload
    // / scenario / params without clobbering. `ended_at_nanos` /
    // `disposition` stay NULL until the shutdown-flush guard updates
    // them. `scope` is non-NULL only under refine.
    {
        let mut cli_keys: Vec<&String> = params.keys().collect();
        cli_keys.sort();
        let cli_text: String = cli_keys
            .iter()
            .filter_map(|k| params.get(*k).map(|v| format!("{k}={v}")))
            .collect::<Vec<_>>()
            .join("\n");
        let scope_for_row: Option<&str> = if refine_requested {
            Some(refine_scope.unwrap_or("missing"))
        } else {
            None
        };
        if let Ok(mut guard) = sqlite_reporter.lock()
            && let Some(r) = guard.as_mut()
        {
            let exec_id = execution.exec_id;
            let sid = session.id.clone();
            r.set_execution_metadata(&sid, exec_id, "workload", &execution.workload);
            r.set_execution_metadata(&sid, exec_id, "scenario", &execution.scenario);
            r.set_execution_metadata(
                &sid,
                exec_id,
                "start_time",
                &format!(
                    "{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs()
                ),
            );
            for (k, v) in &merged_params {
                r.set_execution_metadata(&sid, exec_id, &format!("param.{k}"), v);
            }
            // Reproducibility: the metrics db alone re-creates the run
            // (raw workload YAML + verbatim CLI params).
            if let Some(yaml) = workload_source_text.as_deref() {
                r.set_execution_metadata(&sid, exec_id, "workload_yaml", yaml);
            }
            r.set_execution_metadata(&sid, exec_id, "cli_params", &cli_text);
            r.insert_execution_start(
                &session.id,
                execution.exec_id,
                execution.verb,
                scope_for_row,
                execution.started_at_nanos,
                workload_source_text.as_deref().unwrap_or(""),
                &cli_text,
            );
        }
    }

    // SRD-63 Push 9a: fire `EventType::SessionStart` once at the
    // workload root. Workloads bind structural rows to
    // this slot via `readouts: { on_session_start: … }`;
    // unbound slots stay quiet (no built-in default
    // emission today). Fires whether the run takes the
    // phased or single-activity branch below.
    {
        let session_ctx = crate::readout_context::LifecycleContext {
            event: crate::lifecycle::EventType::SessionStart,
            subject_name: session.id.clone(),
            subject_labels: String::new(),
            depth_indent: String::new(),
            use_color: crate::observer::use_color(),
            stick_reattached: stick_reattached.clone().unwrap_or_default(),
        };
        // SRD-106 D4 — when the stick_session rung engaged, seed
        // the slot with the `session_notice` builtin so the
        // announcement is the first notable event of the run,
        // ahead of any phase event. Workload-bound
        // on_session_start readouts still render after it; runs
        // without stick keep the slot's quiet default.
        let default_body = if stick_reattached.is_some() {
            crate::readouts::parse::bake("session_notice")
                .map(|(body, _)| body)
                .ok()
        } else {
            None
        };
        crate::readout_context::fire_lifecycle(
            crate::lifecycle::EventType::SessionStart,
            &workload_readouts,
            default_body,
            &session_ctx,
            Some(&sqlite_reporter),
        );
    }

    // Merge all ops for param expansion and Polydat compilation.
    let _num_top_level_ops = ops.len();
    let mut all_ops_for_compile: Vec<nmbrs_workload::model::ParsedOp> = ops;
    all_ops_for_compile.extend(phase_ops_for_compile);

    // === Pre-compile rewrites ===
    //
    // Stage 2 (post-M3.6): the workload-params kernel
    // (`crate::params::build_workload_params_kernel`) installs
    // every workload param as a `final <name> := <literal>`
    // binding on the workload-root kernel. Descendant scopes
    // see those bindings via `materialize_wiring_from_outer` + standard GK
    // scope-chain lookup. The legacy
    // `rewrite_workload_param_idents_in_bindings` text pass
    // (which substituted `{name}` → literal value before
    // compilation) was redundant once that path landed and
    // produced broken output for in-string placeholders
    // (`"{dataset}:{profile}"` → `""sift1m":"default""`),
    // so it has been retired.
    //
    // What's left here: rewrite inline `{{expr}}` constructs to
    // named bindings so the Polydat compiler can hoist them as
    // `const __expr_N := …` entries. That pass is a bind-point
    // shape transform, not a value substitution — it operates
    // independently of workload params.
    crate::scope::rewrite_inline_exprs(&mut all_ops_for_compile);

    // The workload-level `bindings:` (top-level YAML block) is
    // a first-class workload-scope input — separate from any
    // op's bindings. We pass it through to the compiler as a
    // distinct source so cursor declarations and other
    // workload-scoped Polydat statements land on the workload kernel
    // alongside the workload params, *without* going through
    // the op-binding param-ident rewrite (which would text-
    // substitute `{name}` placeholders inside string literals).
    // Polydat's standard string-interpolation handles `{name}` at
    // compile time against the `final <name> := <literal>`
    // bindings that workload-params injection installs (M3.6 path).
    // SRD-13f Push D: workload-level `bindings:` reach the
    // workload-root kernel ONLY through this explicit channel
    // now. The parser no longer merges workload bindings into
    // ops (`nmbrs_workload::parse::inline_block_sugar_into_op`
    // is the only remaining parser-time inlining; it operates
    // on block-level YAML sugar, not workload-level). Both
    // BindingsDef forms route through here:
    //   - PolydatSource: pass through verbatim.
    //   - Map: legacy semicolon-chain syntax translated to GK
    //     source lines via `legacy_chain_map_to_polydat_lines`.
    let workload_level_polydat: Option<String> = match &workload.bindings {
        nmbrs_workload::model::BindingsDef::PolydatSource(s) if !s.trim().is_empty() => {
            Some(s.clone())
        }
        nmbrs_workload::model::BindingsDef::Map(m) if !m.is_empty() => Some(
            crate::bindings::legacy_chain_map_to_polydat_lines(m)
                .map_err(|e| format!("workload-level bindings: {e}"))?,
        ),
        _ => None,
    };

    // === Polydat Compilation ===

    let workload_dir: Option<&std::path::Path> = if workload_is_bundled {
        // A catalog name is not a path — don't derive a bogus
        // directory from its namespace segments. Bundled
        // workloads get the cwd as their relative-resolution
        // context (same as inline workloads).
        Some(std::path::Path::new("."))
    } else {
        workload_file
            .as_ref()
            .and_then(|p| std::path::Path::new(p).parent())
            .or_else(|| Some(std::path::Path::new(".")))
    };

    let mut config_refs: Vec<String> = params
        .values()
        .filter(|v| v.starts_with('{') && v.ends_with('}'))
        .map(|v| {
            let mut inner = v[1..v.len() - 1].to_string();
            // Expand workload params in config expressions
            for (key, value) in &workload_params {
                let placeholder = format!("{{{key}}}");
                if inner.contains(&placeholder) {
                    inner = inner.replace(&placeholder, value);
                }
            }
            inner
        })
        .collect();
    for (name, phase) in &phases {
        if phase.for_each.is_some() {
            continue; // for_each phase cycles resolved per-iteration
        }
        if let Some(ref c) = phase.cycles
            && c.starts_with('{')
            && c.ends_with('}')
        {
            let mut inner = c[1..c.len() - 1].to_string();
            for (key, value) in &workload_params {
                let placeholder = format!("{{{key}}}");
                if inner.contains(&placeholder) {
                    inner = inner.replace(&placeholder, value);
                }
            }
            config_refs.push(inner);
        }
        let _ = name; // suppress unused warning
    }

    // Parse limit param for cursor clamping
    let cursor_limit: Option<u64> = merged_params.get("limit").and_then(|s| s.parse().ok());

    // Build the workload-params root kernel first. This is the
    // canonical home for every declared workload parameter —
    // one `final <name> := <literal>` per param, compiled into
    // a stand-alone kernel whose outputs every descendant
    // `materialize_wiring_from_outer`s through. Replaces the prior approach
    // of patching params into multiple places (per-op binding
    // text substitution, per-kernel `final` injection in
    // `build_scope`). See `nmbrs-runtime::params`.
    // `build_workload_params_kernel` already prefixes its error
    // with "workload params kernel:" and appends the generated
    // source for diagnosis — propagate it verbatim rather than
    // re-wrapping (which doubled the prefix and dropped the
    // source dump).
    let params_kernel = crate::params::build_workload_params_kernel(&workload_params)?;

    // Build the workload kernel directly as a subscope of the
    // params kernel via the typed PolydatKernel-controlled
    // construction path. Cells from params flow in via the
    // cascade — no late-binding step required.
    // Build the workload kernel as a subscope of the params
    // kernel and share ONE Arc across both consumers (the
    // scope tree's canonical reference AND the OpBuilder's
    // source kernel). A second materialize_subscope would
    // produce a sibling kernel with its own freshly-seeded
    // shared cells — disconnected from the canonical, so
    // result-binding writes from one chain wouldn't be visible
    // to the other. Sharing the Arc keeps the cell handles
    // identical end-to-end: detect_dialect's writes reach
    // await_index's reads through the same Mutex<Value>.
    // SRD-13f Push D: the workload-root kernel still owns the
    // canonical workload-scope bindings, but descendant kernels
    // (phase kernel, op-template kernel) carry a cascade copy
    // of the workload-level Polydat source as local matter — so
    // fiber.main_kernel evaluates dynamic workload bindings
    // (e.g. cycle-dependent) on its own state per cycle. See
    // `compile_from_scope` (and its callers in
    // `executor.rs::run_phase`) for the cascade-copy plumbing.
    // No eager pull at workload-root construction: that would
    // (a) fire side-effecting nodes like `testkit_throw_at` outside the
    // phase cascade context, and (b) cache stale values for
    // cycle-dependent bindings.
    let workload_canonical_kernel: std::sync::Arc<polydat::kernel::PolydatKernel> =
        std::sync::Arc::new(
            build_workload_root_kernel(
                &params_kernel,
                &all_ops_for_compile,
                workload_dir,
                polydat_lib_paths.clone(),
                strict,
                &config_refs,
                "outer workload bindings",
                cursor_limit,
                &workload_params,
                workload_level_polydat.as_deref(),
            )
            .map_err(|e| format!("outer workload bindings: {e}"))?,
        );
    let kernel = workload_canonical_kernel.clone();

    // Extract output manifest and folded constant values from outer kernel
    // === Polydat Config Resolution (all done before kernel is consumed) ===
    // (The `cycles=` / `concurrency=` resolution that used to
    // happen here fed the now-deleted single-activity branch.
    // The phased path resolves these per-phase inside
    // `run_phase` via the phase-scope Polydat Kernel.)

    // Collect phases that are inside scenario for_each groups — these have
    // iteration variables resolved at runtime, not pre-resolution time.
    fn collect_grouped_phases(
        nodes: &[nmbrs_workload::model::ScenarioNode],
        in_group: bool,
        out: &mut std::collections::HashSet<String>,
    ) {
        for node in nodes {
            match node {
                nmbrs_workload::model::ScenarioNode::Phase(name) => {
                    if in_group {
                        out.insert(name.clone());
                    }
                }
                nmbrs_workload::model::ScenarioNode::Comprehension { children, .. }
                | nmbrs_workload::model::ScenarioNode::DoWhile { children, .. }
                | nmbrs_workload::model::ScenarioNode::DoUntil { children, .. } => {
                    collect_grouped_phases(children, true, out);
                }
                nmbrs_workload::model::ScenarioNode::IncludedScenario { children, .. } => {
                    // Inclusion is transparent — children inherit
                    // whatever grouping context wrapped the
                    // include site. We pass `in_group` through
                    // so a `scenario:` reference at top level of
                    // a scenario doesn't artificially mark its
                    // phases as grouped.
                    collect_grouped_phases(children, in_group, out);
                }
                nmbrs_workload::model::ScenarioNode::Bindings { children, .. } => {
                    // Scenario-tree `bindings:` (and the `set:`
                    // sugar that lowers to it) is transparent
                    // for grouping — it doesn't introduce
                    // iteration. Pass `in_group` through.
                    collect_grouped_phases(children, in_group, out);
                }
            }
        }
    }
    let mut grouped_phases = std::collections::HashSet::new();
    for nodes in scenarios.values() {
        collect_grouped_phases(nodes, false, &mut grouped_phases);
    }

    // Pre-resolve phase cycles (skip phases with for_each or in scenario groups)
    let mut resolved_phase_cycles: HashMap<String, Option<u64>> = HashMap::new();
    for (name, phase) in &phases {
        if phase.for_each.is_some() || grouped_phases.contains(name) {
            continue;
        }
        let resolved = phase.cycles.as_ref().and_then(|s| {
            let expanded = expand_workload_params(s, &workload_params);
            resolve_polydat_config(&expanded, &kernel)
        });
        resolved_phase_cycles.insert(name.clone(), resolved);
    }

    // Strip workload-level adapter/driver from op params
    // (adapter is resolved per-phase/per-op, not from workload params)
    for op in &mut all_ops_for_compile {
        op.params.remove("adapter");
        op.params.remove("driver");
    }
    for ops in phase_raw_ops.values_mut() {
        for op in ops.iter_mut() {
            op.params.remove("adapter");
            op.params.remove("driver");
        }
    }

    let builder = Arc::new(OpBuilder::new(kernel));
    let program = builder.program();

    // Unification — the scenario-tree executor is the sole
    // execution path. `Workload::synthesize_default_phase` (called
    // at load time) guarantees `phases` is non-empty for any
    // workload that has work to do; the empty-ops error fired
    // earlier in this fn covers the "literally nothing to run"
    // case. The legacy single-activity branch is gone.
    {
        // --- Phased execution ---
        let scenario_name = params
            .get("scenario")
            .map(|s| s.as_str())
            .unwrap_or("default");
        let scenario_nodes = resolve_scenario(&scenarios, &phase_order, scenario_name)?;

        // Build the canonical scope tree (SRD 18b §"Canonical
        // traversal"). Mirrors the scenario tree 1:1 with parent
        // pointers, depth, and pragma slots. Today consumed by
        // observer pre-mapping and diagnostic display; future
        // steps drive execution from this tree directly.
        let scope_tree = {
            let mut t = crate::scope_tree::ScopeTree::build(scenario_name, &scenario_nodes);
            // Populate phase-leaf pragmas from each phase's GK
            // source and chain each scope's `PragmaSet` to its
            // parent's. SRD 18b §"Pragma chain along the scope
            // tree"; SRD 15 §"Pragma Scope".
            let conflicts = t.populate_pragmas(&phases);
            for c in &conflicts {
                let path = t
                    .ancestors(c.scope_idx)
                    .map(|(_, n)| n.kind.label())
                    .collect::<Vec<_>>()
                    .join(" ← ");
                let msg = format!(
                    "pragma '{}' conflict at {path}: outer (line {}) overrides inner (line {})",
                    c.name, c.outer_line, c.inner_line,
                );
                if strict {
                    return Err(msg);
                } else {
                    crate::diag!(crate::observer::LogLevel::Warn, "{msg}");
                }
            }
            // Validate iter-var name uniqueness against workload
            // params and enclosing iter vars. Aliasing creates an
            // unambiguous spec-evaluation case the runtime can't
            // disambiguate; reject up-front.
            let wp_names: std::collections::HashSet<String> =
                workload_params.keys().cloned().collect();
            t.validate_iter_var_uniqueness(&wp_names)?;
            // SRD-83 follow-up — load-time authoring lints: every
            // `errors:` router spec must parse (bad verbs fail the
            // load, not the first runtime error), a router without a
            // catch-all rule warns, and `metric()` families in
            // stop/gate predicates are checked against the instrument
            // namespace (an unregistered family reads 0.0 silently).
            for w in crate::workload_lint::lint_workload(
                &workload.stop_when,
                phases.iter().map(|(k, v)| (k.as_str(), v)),
            )? {
                crate::diag!(crate::observer::LogLevel::Warn, "{w}");
            }
            // SRD-13d Phase 6 — extend the scope tree with
            // op-template children of every Phase node so the
            // op tier is visible to the elision classifier
            // and downstream diagnostics.
            t.extend_with_op_templates(&phases);
            // SRD-13d Phase 3 — workload-init scope-elision
            // pre-walk. Reads `HasGkMatter` on each AST node
            // and marks the corresponding scope-tree node
            // `materialised` (own kernel) or elided (binds
            // through parent). Conservative predicate today
            // (Definitions ⇒ materialise without hash-subset
            // refinement); Phase 6 tightens it.
            //
            // Scoped fields rather than `&workload` because
            // `workload.ops` was moved earlier in this fn —
            // the classifier reads only bindings + params +
            // phases anyway.
            let classify_inputs = crate::scope_elision::ClassifyInputs {
                bindings: &workload.bindings,
                params: &workload.params,
                phases: &phases,
            };
            crate::scope_elision::classify_and_mark(&mut t, &classify_inputs);
            std::sync::Arc::new(t)
        };

        // dryrun=kernels: register the ride-along visitor BEFORE
        // the install loop so every install_kernel call (root
        // workload kernel + per-scope kernels in DFS pre-order)
        // streams its polydat source to stdout. Cleared after
        // the install loop runs (see the matching short-circuit
        // a few hundred lines below).
        if params.get("dryrun").map(|s| s.as_str()) == Some("kernels") {
            print_kernel_dump_header();
            crate::scope_tree::set_kernel_install_visitor(Some(Box::new(|node, _idx, kernel| {
                print_kernel_for_scope(node, kernel);
            })));
        }

        // Install the workload-params module on the session node
        // (the workload root's parent) so identity chains
        // (`ScopeTree::ancestor_kernels` → the SRD-44/SRD-77
        // provenance hashes) include the module whose const
        // slots carry every param VALUE. The workload kernel is
        // built as a subscope of this module; installing it here
        // records that same relationship on the tree. Nearest-
        // ancestor kernel lookups are unaffected — the workload
        // root always has its own kernel, so walks stop there.
        scope_tree.install_kernel(scope_tree.root, std::sync::Arc::new(params_kernel));

        // Install the canonical workload kernel (SRD 18b §"Iter
        // vars as scope outputs"). After this, intermediate
        // scopes (for_each, for_combinations, …) install their
        // own kernels in DFS pre-order below — each one's
        // synthesis reads its parent's manifest via the standard
        // Polydat API on the parent's installed kernel.
        scope_tree.install_kernel(scope_tree.workload_root_idx(), workload_canonical_kernel);

        // M3.2: install per-scope kernels for for_each /
        // for_combinations nodes. Each kernel re-exports its
        // iteration variables and any referenced inherited
        // values as outputs (`const x := x` passthrough), so
        // children's standard `materialize_wiring_from_outer(parent)`
        // chains inheritance through arbitrary nesting depth
        // — no caller-side scope-tree walking for name
        // resolution at runtime.
        let workload_dir_owned: Option<std::path::PathBuf> = workload_dir.map(|p| p.to_path_buf());
        // M3.4b: scope kinds get categorized for synthesis.
        // For-comprehensions (ForEach, ForCombinations,
        // ForEachUnion) carry tuple iteration vars; do-loops
        // (DoWhile, DoUntil) carry an optional counter +
        // condition expression. Both produce installed kernels
        // that the unified dispatch_comprehension reads from.
        // reason: function-local, short-lived spec list built once per
        // install pass; the `OpTemplate`/`ParsedOp` variant dominates the
        // size, but boxing it would ripple `Box::new` + deref across every
        // construction and destructuring match arm below for no real gain
        // on a vector that is consumed immediately.
        #[allow(clippy::large_enum_variant)]
        enum InstallSpec {
            ForComprehension {
                idx: crate::scope_tree::ScopeNodeIdx,
                iter_vars: Vec<String>,
                spec_exprs: Vec<String>,
                /// SRD-13f Push E: phase-level `bindings:` folded
                /// into the for_each scope kernel when the phase
                /// declares both `for_each:` AND `bindings:`. The
                /// single install at the phase node materializes
                /// one kernel carrying both the iter-var
                /// declarations AND the phase-level bindings.
                /// Empty for pure-comprehension scope nodes
                /// (scenario-level `for_each:`) and for phase
                /// nodes without own bindings.
                phase_bindings: nmbrs_workload::model::BindingsDef,
            },
            DoLoop {
                idx: crate::scope_tree::ScopeNodeIdx,
                counter: Option<String>,
                condition: String,
            },
            /// SRD-13d Phase 9 — install a kernel for an
            /// op-template scope that classified as
            /// `materialised`. Flattened op-templates
            /// (`materialised == false`) get no install spec;
            /// their dispensers reach the parent kernel via
            /// `nearest_materialised`.
            OpTemplate {
                idx: crate::scope_tree::ScopeNodeIdx,
                op: nmbrs_workload::model::ParsedOp,
            },
            /// SRD-13d Phase 9 — install a phase-scope kernel
            /// for a phase that declares its own `bindings:`
            /// block (and no `for_each:` — that case is owned
            /// by the for_each install spec at the same node).
            /// Phases without bindings AND without for_each
            /// emit no install spec; the closure-lifetime
            /// kernel reference is the parent's by walker
            /// fall-through.
            /// Phase-scope kernel install for phases declaring
            /// their own `bindings:` block (and no `for_each:` —
            /// that case is owned by the for_each install spec
            /// at the same node). Also covers scenario-tree
            /// `bindings:` nodes (and the `set:` sugar that
            /// lowers to them) — the synthesizer is identical
            /// for both: parent-cascaded externs + the body's
            /// authored matter.
            Bindings {
                idx: crate::scope_tree::ScopeNodeIdx,
                bindings: nmbrs_workload::model::BindingsDef,
            },
        }
        let install_specs: Vec<InstallSpec> = scope_tree
            .iter_dfs()
            .filter_map(|(idx, node)| match &node.kind {
                crate::scope_tree::ScopeKind::Comprehension { comprehension } => {
                    // Representative iter_vars + spec_exprs for
                    // synthesis: dedup'd by var name. Walks the
                    // algebra AST directly via coordinate_specs
                    // — same dedup semantics the legacy
                    // scalar_bindings/Union-flatten path
                    // produced.
                    let pairs = comprehension.coordinate_specs();
                    let vars: Vec<String> = pairs.iter().map(|(v, _)| v.clone()).collect();
                    let specs: Vec<String> = pairs.iter().map(|(_, e)| e.clone()).collect();
                    Some(InstallSpec::ForComprehension {
                        idx,
                        iter_vars: vars,
                        spec_exprs: specs,
                        // Scope-tree Comprehension nodes (scenario-
                        // level `for_each:`) carry no phase-level
                        // bindings; the wrapping phase has its own
                        // scope-tree node and its own install spec
                        // (PhaseBindings or another ForComprehension).
                        phase_bindings: nmbrs_workload::model::BindingsDef::default(),
                    })
                }
                crate::scope_tree::ScopeKind::DoWhile { condition, counter } => {
                    Some(InstallSpec::DoLoop {
                        idx,
                        counter: counter.clone(),
                        condition: condition.clone(),
                    })
                }
                crate::scope_tree::ScopeKind::DoUntil { condition, counter } => {
                    Some(InstallSpec::DoLoop {
                        idx,
                        counter: counter.clone(),
                        condition: condition.clone(),
                    })
                }
                crate::scope_tree::ScopeKind::Phase { name } => {
                    // Phase-scope kernel installation, matter-gated
                    // per SRD-13d / SRD-67. Three cases:
                    //
                    //   1. Phase declares `for_each:` — treat as
                    //      a single-clause tuple comprehension.
                    //      The for_each scope owns the phase
                    //      node's kernel; phase `bindings:` (if
                    //      also present) need to fold into that
                    //      scope's matter (deferred — the legacy
                    //      parser-merge path keeps the bindings
                    //      reachable via op-bindings until the
                    //      for_each-with-bindings synthesizer
                    //      lands).
                    //
                    //   2. Phase declares only `bindings:` — own
                    //      subscope from those bindings layered
                    //      over the parent kernel.
                    //
                    //   3. Neither — no install spec; phase
                    //      scope's closure inherits the parent's
                    //      kernel reference via the walker's
                    //      fall-through (the matter-gated
                    //      pass-through).
                    let phase = phases.get(name.as_str())?;
                    if let Some(spec) = phase.for_each.as_ref() {
                        if !phase.metrics.is_empty() {
                            // Phase-level `metrics:` + phase-level
                            // `for_each:` isn't supported in the
                            // initial ship: the for_each scope
                            // synthesiser doesn't yet thread the
                            // metric-binding augmentation through, and
                            // the per-iteration completion-pull
                            // semantics want their own design pass.
                            // Reject loudly rather than silently
                            // dropping the metrics.
                            crate::diag!(
                                crate::observer::LogLevel::Error,
                                "phase '{name}': phase-level `metrics:` + phase-level \
                                 `for_each:` is not supported yet. Move the for_each to \
                                 scenario-tree level (so each iteration is its own phase \
                                 activation, each with its own metrics), or drop one. \
                                 Phase will be skipped.",
                            );
                            return None;
                        }
                        if phase.poll.is_some() {
                            // SRD-75: phase-poll + phase-level
                            // for_each isn't supported in the
                            // initial ship. The for_each scope
                            // synthesizer doesn't yet thread the
                            // poll augmentation through, and the
                            // combination's semantics
                            // (iterate-and-synchronize-each-cell?
                            // iterate-while-synchronizing?) wants
                            // its own design pass. Reject loudly.
                            crate::diag!(
                                crate::observer::LogLevel::Error,
                                "phase '{name}': `poll:` + phase-level `for_each:` is not \
                                 supported in the initial ship of SRD-75. Move the for_each \
                                 to scenario-tree level (so each iter is its own phase \
                                 activation), or drop one of the two. Phase will be skipped.",
                            );
                            return None;
                        }
                        // Delegate the for_each grammar to polydat (the
                        // single owner): `parse_inline` handles single- AND
                        // multi-clause, and `coordinate_specs()` yields the
                        // per-clause (var, spec) pairs — identical to the
                        // scenario-level path above. A single-clause spec
                        // yields exactly one pair, preserving prior behaviour.
                        let comp = match polydat::iteration::comprehension::spec::parse_inline(spec)
                        {
                            Ok(c) => c,
                            Err(e) => {
                                crate::diag!(
                                    crate::observer::LogLevel::Error,
                                    "phase '{name}' for_each '{spec}': {e}"
                                );
                                return None;
                            }
                        };
                        let pairs = comp.coordinate_specs();
                        let iter_vars: Vec<String> = pairs.iter().map(|(v, _)| v.clone()).collect();
                        let spec_exprs: Vec<String> =
                            pairs.iter().map(|(_, e)| e.clone()).collect();
                        // SRD-13f Push E: phases declaring both `for_each:`
                        // and `bindings:` fold the bindings into the for_each
                        // scope kernel (one kernel, one install). Pure-for_each
                        // phases pass an empty `BindingsDef` (no-op).
                        //
                        // Route through the SAME phase-scope synthesis the
                        // non-for_each branch uses, so phase-level `metrics:` /
                        // `poll:` and an inline `optimize.objective` (SRD-86)
                        // are folded into the for_each kernel too — not just the
                        // author's raw `bindings:`. The synthesizer returns the
                        // raw bindings unchanged when there is nothing to add,
                        // so plain for_each phases are unaffected.
                        let phase_bindings =
                            match crate::scope::synthesize_phase_scope_bindings(phase) {
                                Ok(b) => b,
                                Err(e) => {
                                    crate::diag!(
                                        crate::observer::LogLevel::Error,
                                        "phase '{name}': phase-scope synthesis: {e}"
                                    );
                                    return None;
                                }
                            };
                        Some(InstallSpec::ForComprehension {
                            idx,
                            iter_vars,
                            spec_exprs,
                            phase_bindings,
                        })
                    } else {
                        // SRD-75: when the phase declares `poll:`,
                        // synthesise capture-as-shared-cell
                        // declarations + the `__poll_until`
                        // predicate binding into the phase scope's
                        // bindings, even when the phase has no
                        // author-declared `bindings:` of its own.
                        // The synthesised bindings flow through the
                        // same `build_phase_scope_kernel` path as
                        // any other phase-level bindings; phase-
                        // poll has no synthesizer-specific code
                        // path.
                        let synth = match crate::scope::synthesize_phase_scope_bindings(phase) {
                            Ok(b) => b,
                            Err(e) => {
                                crate::diag!(
                                    crate::observer::LogLevel::Error,
                                    "phase '{name}': SRD-75 phase-poll synthesis: {e}",
                                );
                                return None;
                            }
                        };
                        if !synth.is_empty() {
                            Some(InstallSpec::Bindings {
                                idx,
                                bindings: synth,
                            })
                        } else {
                            None
                        }
                    }
                }
                crate::scope_tree::ScopeKind::OpTemplate { name } => {
                    // SRD-13d Phase 9: install a per-op kernel
                    // ONLY for materialised op-templates. The
                    // scope-elision pre-walk already set the
                    // mark; we just gate on it here.
                    if node.materialised != Some(true) {
                        return None;
                    }
                    // Find the ParsedOp by walking up to the
                    // OWNING phase first, then resolving by name
                    // within that phase. Two phases can both
                    // declare an op named e.g. `select_ann` with
                    // very different bodies; a flat
                    // `phases.values().flat_map(|p| p.ops.iter())
                    // .find(...)` would pick whichever phase the
                    // HashMap iterator yielded first, silently
                    // compiling pvs_query's body into ann_query's
                    // op-template kernel (and vice versa).
                    let owning_phase: Option<&str> = {
                        let mut cursor = scope_tree.nodes[idx].parent;
                        let mut found: Option<&str> = None;
                        while let Some(p) = cursor {
                            if let crate::scope_tree::ScopeKind::Phase { name: pname } =
                                &scope_tree.nodes[p].kind
                            {
                                found = Some(pname.as_str());
                                break;
                            }
                            cursor = scope_tree.nodes[p].parent;
                        }
                        found
                    };
                    owning_phase
                        .and_then(|pname| phases.get(pname))
                        .and_then(|phase| phase.ops.iter().find(|op| op.name == *name))
                        .cloned()
                        .map(|op| InstallSpec::OpTemplate { idx, op })
                }
                crate::scope_tree::ScopeKind::Bindings { source } => {
                    // Scenario-tree `bindings:` block (also the
                    // canonical lowered form of `set:` sugar)
                    // installs through the same synthesizer
                    // phases use for their own `bindings:`. The
                    // body source compiles into a scope kernel
                    // that publishes its `final`/`init`/cycle
                    // bindings as outputs; descendants read
                    // those through the canonical scope chain
                    // (no HashMap merges, no side-channel
                    // resolvers). Shadowing of upstream names is
                    // enforced by the local-final transit-
                    // suppression rule in
                    // `materialize_wiring_from_outer` — uniform
                    // with every other scope.
                    Some(InstallSpec::Bindings {
                        idx,
                        bindings: nmbrs_workload::model::BindingsDef::PolydatSource(source.clone()),
                    })
                }
                _ => None,
            })
            .collect();

        for install_spec in install_specs {
            let idx = match &install_spec {
                InstallSpec::ForComprehension { idx, .. } => *idx,
                InstallSpec::DoLoop { idx, .. } => *idx,
                InstallSpec::OpTemplate { idx, .. } => *idx,
                InstallSpec::Bindings { idx, .. } => *idx,
            };
            // Nearest installed ancestor — skips Scenario /
            // IncludedScenario nodes that don't install kernels
            // (those are pass-through structural).
            let parent_kernel = {
                let mut cursor = scope_tree.nodes[idx].parent;
                let mut found: Option<std::sync::Arc<polydat::kernel::PolydatKernel>> = None;
                while let Some(p) = cursor {
                    if let Some(k) = scope_tree.nodes[p].cached_kernel.get() {
                        found = Some(k.clone());
                        break;
                    }
                    cursor = scope_tree.nodes[p].parent;
                }
                found.expect("workload root always has an installed kernel")
            };
            let parent_manifest = extract_manifest(parent_kernel.program());
            let context = format!("scope idx {idx} ({})", scope_tree.nodes[idx].kind.label(),);

            let result = match install_spec {
                InstallSpec::ForComprehension {
                    iter_vars,
                    spec_exprs,
                    phase_bindings,
                    ..
                } => {
                    let bindings: Vec<(String, String)> = iter_vars
                        .iter()
                        .cloned()
                        .zip(spec_exprs.iter().cloned())
                        .collect();
                    // SRD-13f Push E: translate phase-level
                    // `bindings:` into the Polydat source the
                    // for_each synthesiser folds in. PolydatSource
                    // form passes verbatim; Map form serialises
                    // to `name := expr\n` lines.
                    let phase_bindings_source = match phase_bindings {
                        nmbrs_workload::model::BindingsDef::PolydatSource(s)
                            if !s.trim().is_empty() =>
                        {
                            Some(s)
                        }
                        nmbrs_workload::model::BindingsDef::Map(m) if !m.is_empty() => {
                            let mut out = String::new();
                            for (name, expr) in &m {
                                out.push_str(&format!("{name} := {expr}\n"));
                            }
                            Some(out)
                        }
                        _ => None,
                    };
                    crate::scope_synth::build_for_each_scope_kernel(
                        &bindings,
                        &parent_manifest,
                        &parent_kernel,
                        &workload_params,
                        polydat_lib_paths.clone(),
                        workload_dir_owned.as_deref(),
                        strict,
                        &context,
                        phase_bindings_source.as_deref(),
                    )
                }
                InstallSpec::DoLoop {
                    counter, condition, ..
                } => crate::scope::build_do_loop_scope_kernel(
                    counter.as_deref(),
                    &condition,
                    &parent_manifest,
                    &parent_kernel,
                    &workload_params,
                    polydat_lib_paths.clone(),
                    workload_dir_owned.as_deref(),
                    strict,
                    &context,
                ),
                InstallSpec::OpTemplate { op, .. } => {
                    // SRD-13d Phase 9 — synthesize the op-
                    // template kernel layered over the parent.
                    // Includes op-level bindings + cascaded
                    // parent externs; materialize_wiring_from_outer chains
                    // values in at runtime.
                    crate::scope::build_op_template_scope_kernel(
                        &op,
                        &parent_manifest,
                        &parent_kernel,
                        &workload_params,
                        polydat_lib_paths.clone(),
                        workload_dir_owned.as_deref(),
                        strict,
                        kernel_opt,
                        &context,
                    )
                }
                InstallSpec::Bindings { bindings, .. } => {
                    // Single install path for both phase-level
                    // `bindings:` and scenario-tree-level
                    // `bindings:` (including the `set:` sugar
                    // form that lowers to it). The synthesizer
                    // cascades workload params + parent
                    // outputs/inputs as externs and appends the
                    // body verbatim; Polydat handles workload-param
                    // interpolation and expression evaluation
                    // at compile time. Lexical shadowing of an
                    // upstream `final NAME` by the body's own
                    // `const NAME := …` is enforced by the
                    // local-final transit-suppression rule in
                    // `materialize_wiring_from_outer`.
                    crate::scope::build_phase_scope_kernel(
                        &bindings,
                        &parent_manifest,
                        &parent_kernel,
                        &workload_params,
                        polydat_lib_paths.clone(),
                        workload_dir_owned.as_deref(),
                        strict,
                        &context,
                    )
                }
            };

            match result {
                Ok(kernel) => {
                    let _ = scope_tree.install_kernel(idx, std::sync::Arc::new(kernel));
                }
                Err(e) => {
                    // Kernel synthesis failure is a hard error
                    // regardless of strict mode: a phase whose GK
                    // source doesn't compile literally cannot run.
                    // Letting the walk continue past the failure
                    // only delays the bad news — the phase will
                    // fail mid-run with a less helpful diagnostic
                    // (or, worse, run with stale / partial
                    // kernels installed for sibling scopes). The
                    // earlier "warn-and-continue" behavior dates
                    // from before strict mode existed; with the
                    // strict-mode behavior being the only sane
                    // default, the non-strict branch was
                    // effectively a footgun that turned compile
                    // errors into silent partial runs.
                    return Err(format!("scope kernel synthesis failed: {e}"));
                }
            }
        }

        crate::diag!(
            crate::observer::LogLevel::Info,
            "scenario '{scenario_name}':\n{}",
            format_scenario_tree(&scenario_nodes, &phases)
        );

        // Observer is passed from the caller (default: StderrObserver).

        // ─── Unified walker: structural pre-map pass ─────────────────
        //
        // Per SRD 18b §"Single Walker Contract", there is ONE walker
        // function (`crate::executor::execute_tree`). It runs twice
        // here: first at depth=Phase to populate the scene tree (so
        // resume_plan / declare_scene_tree_phases / pre_map_pending_uses
        // can read the populated tree), then again at the configured
        // depth to actually execute. SceneTree::push is idempotent
        // by `(parent, kind, name)` so the second pass re-encounters
        // every node from the first without duplicating.
        //
        // The pre-map ExecCtx uses stub post-pre-map fields
        // (checkpoint_writer = None, fresh resume_plan, fresh
        // resource_pool); they're updated to the real values after
        // pre-map produces the tree.
        let schedule_spec = std::sync::Arc::new(match params.get("schedule") {
            Some(s) => crate::scheduler::ScheduleSpec::parse(s)
                .map_err(|e| format!("schedule= param: {e}"))?,
            None => crate::scheduler::ScheduleSpec::default_serial(),
        });
        // `&str` → `&'static str` so the activity config can
        // carry the mode label across thread boundaries
        // without lifetime gymnastics. Every mode the resolver
        // above produces must appear here, otherwise the
        // wrapper-install path silently sees `None` and
        // DRYRUN never installs.
        let dry_run_static: Option<&'static str> = match dry_run {
            Some("silent") => Some("silent"),
            Some("fields") => Some("fields"),
            Some("cycle") => Some("cycle"),
            Some("op") => Some("op"),
            _ => None,
        };

        // phases=<pattern> filter (bareword / glob / regex). When
        // unset, every phase runs. When set, the planner's
        // scenario-tree walker skips phase activations whose name
        // doesn't match AND elides scope subtrees with no
        // matching descendant.
        let phase_filter: Option<Arc<crate::phase_filter::PhasePattern>> = match params
            .get("phases")
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
        {
            None => None,
            Some(src) => {
                let pat = crate::phase_filter::PhasePattern::parse(src)
                    .map_err(|e| format!("phases= param: {e}"))?;
                crate::diag!(
                    crate::observer::LogLevel::Info,
                    "phases=<filter>: pattern '{src}' ({}{})",
                    if pat.negated() { "negated " } else { "" },
                    pat.dialect().as_str()
                );
                Some(Arc::new(pat))
            }
        };
        let resource_pool = Arc::new(crate::resource_pool::ResourcePool::new());
        // SRD-104 — install the process-global resource-accessor bridge and
        // point it at this session's pool, so kernel nodes can reach a live
        // pool-owned resource (e.g. a CQL session handle) by fingerprint via
        // `polydat::resource_lookup`. The pool stays the definitive owner.
        crate::resource_pool::install_accessor(&resource_pool);
        let initial_scene_tree_path = vec![crate::checkpoint::PathSegment::Scenario(
            scenario_name.to_string(),
        )];
        // SRD-82 — the session root error policy. Every shell resolves
        // its own from this (inherit or derive); equal configs share
        // one instance, parsed once per session.
        let root_error_policy = crate::error_policy::ErrorPolicy::root(
            crate::error_policy::PolicyConfig::new(error_spec.clone(), error_rate_max),
        );
        // SRD-83 — build the workload execution shell (SRD-82's
        // outermost shell). Its stop conditions are the workload's
        // `stop_when:` declarations whose `each:` names the workload
        // itself (`self`/`workload`), compiled once against the
        // workload root's cached kernel — the same native-scope binding
        // every other shell uses, never a conjured root. The remaining
        // `each: phase` declarations fan out to the per-phase activity
        // build (see `executor.rs`); the unfiltered list rides on
        // `ExecCtx.workload_stop_when` for that gathering.
        //
        // The error-rate breach stays a per-phase concern (each phase
        // already trips on its own `error_rate_max`), so no default
        // error-rate condition is installed at the workload aggregate.
        let workload_shell = {
            use nmbrs_workload::model::ScopeLevel;
            // SRD-82 Part 3/6 — the scenario-graph default `*Failed:stop`:
            // any child phase whose outcome is Failed halts the remaining
            // walk and records `Interrupted + Failed` (a fault). Expressed
            // as the SRD-83 stop condition `children_failed > 0` with a
            // `fail` effect, so it rides the same workload-shell mechanism
            // as declared conditions and reaches concurrent / cross-subtree
            // siblings the local `Err` cascade can't. First in the list →
            // a failed child trips it before any declared graceful rule.
            let mut conditions: Vec<crate::stop_conditions::StopConditionDecl> =
                vec![crate::stop_conditions::StopConditionDecl {
                    when: "children_failed > 0".to_string(),
                    effect: crate::phase_outcome::Outcome::failed(),
                    reason: None,
                    target: crate::stop_conditions::StopScope::Workload,
                    // The default stop-on-error drains cooperatively.
                    cancel_ops: false,
                }];
            // Declared workload-level conditions (`each ∋ self|workload`).
            // A declared trip defaults to a graceful `stop`
            // (Interrupted+Succeeded): nothing failed, later phases are
            // deliberately skipped.
            conditions.extend(
                workload
                    .stop_when
                    .iter()
                    .filter(|c| {
                        c.each
                            .iter()
                            .any(|l| matches!(l, ScopeLevel::SelfScope | ScopeLevel::Workload))
                    })
                    .map(|c| crate::stop_conditions::StopConditionDecl {
                        // Same `{param}` interpolation as phase-level stop_when
                        // (executor build_activity_config_for_phase) so a workload
                        // breaker threshold can be a modular workload param.
                        when: expand_workload_params(&c.when, &workload_params),
                        effect: crate::stop_conditions::StopConditionDecl::effect_from_str(
                            c.effect.as_deref(),
                            crate::phase_outcome::Outcome::interrupted(),
                        ),
                        reason: None,
                        // Detected at the workload shell; `at:` (default =
                        // innermost of `per:`/`each:`, here `workload`) selects
                        // the action scope.
                        target: crate::executor::resolve_stop_scope(c.at, &c.each),
                        // `action: abort` → cancel in-flight ops at the trip site.
                        cancel_ops: crate::stop_conditions::StopConditionDecl::action_cancels_ops(
                            c.effect.as_deref(),
                        ),
                    }),
            );
            let set = match scope_tree.nodes[scope_tree.workload_root_idx()]
                .cached_kernel
                .get()
            {
                Some(root_kernel) => crate::stop_conditions::StopConditionSet::build_for_phase(
                    root_kernel,
                    &conditions,
                )
                .unwrap_or_else(|e| {
                    crate::diag!(
                        crate::observer::LogLevel::Error,
                        "workload stop-condition compile failed: {e}"
                    );
                    crate::stop_conditions::StopConditionSet::empty()
                }),
                _ => crate::stop_conditions::StopConditionSet::empty(),
            };
            std::sync::Arc::new(crate::workload_shell::WorkloadShell::new(set))
        };

        // SRD-71 P3 — phase-scoped CLI parameter overrides
        // (`<phase-pattern>.<param>=<value>`). Parsed from the raw
        // args (parse_params skips dotted keys), validated against
        // the declared phase names so a never-matching pattern is
        // a startup error instead of a silent no-op.
        let phase_param_overrides =
            std::sync::Arc::new(crate::phase_params::parse_overrides(&args)?);
        crate::phase_params::validate_against_phases(
            &phase_param_overrides,
            phases.keys().map(|s| s.as_str()),
        )?;

        let mut exec_ctx = crate::executor::ExecCtx {
            phases: phases.clone(),
            optimize_objective: None,
            optimize_objective_value: None,
            optimize_servo: None,
            phase_param_overrides,
            workload_shell,
            workload_stop_when: workload.stop_when.clone(),
            daemon_stop: None,
            workload_readouts: workload_readouts.clone(),
            cli_readout_override: cli_readout_override.clone(),
            workload_params: workload_params.clone(),
            wrappers_override: workload_wrappers_override.clone(),
            wrap_default_order: cli_wrap_default_order.clone(),
            program: program.clone(),
            polydat_lib_paths: polydat_lib_paths.clone(),
            workload_dir: workload_dir.map(|p| p.to_path_buf()),
            strict,
            driver: driver.clone(),
            merged_params: merged_params.clone(),
            dry_run: dry_run_static,
            phase_filter: phase_filter.clone(),
            refine_plan: refine_plan.clone(),
            diag: {
                let mut d = diag.clone();
                d.depth = ExecDepth::Phase;
                d
            },
            // The pre-map pass walks at depth=Phase but is NOT
            // execution: the structural-only sentinel that fires
            // `set_phase_running` + `_completed` in the walker
            // (intended for the dryrun=phase summary) is
            // suppressed via this flag so the TUI's scene tree
            // doesn't start life with every phase already
            // Completed. Flipped back to false at line ~2675
            // before the real execution pass.
            pre_map_only: true,
            seq_type,
            concurrency,
            rate,
            error_spec: error_spec.clone(),
            tries,
            error_rate_max,
            error_policy: root_error_policy,
            session_id: session_id.clone(),
            exec_id,
            workload_name: execution.workload.clone(),
            label_stack: Vec::new(),
            // SRD-88 §2 — phase/activity components attach under the
            // EXECUTION component (which declares `exec_id` +
            // `workload`), not the session root. The session root
            // is the shared `session=<id>` ancestor above it.
            session_component: execution.component.clone(),
            cadence_reporter: cadence_reporter.clone(),
            stop_handle: stop_handle.clone(),
            observer: observer.clone(),
            scope_tree: scope_tree.clone(),
            schedule_spec: schedule_spec.clone(),
            current_parent_kernel: scope_tree.nodes[scope_tree.workload_root_idx()]
                .cached_kernel
                .get()
                .cloned(),
            workload_source: workload_file.as_ref().and_then(|path| {
                workload_source_text.as_ref().map(|text| {
                    std::sync::Arc::new(crate::executor::WorkloadSource {
                        path: path.clone(),
                        text: text.clone(),
                    })
                })
            }),
            // Stub post-pre-map fields: replaced after the pre-map
            // walk populates the scene tree. Pre-map walks at depth
            // Phase, so no run_phase / run_do_loop / checkpoint
            // events fire — the stubs are not consulted.
            checkpoint_writer: None,
            resume_plan: std::sync::Arc::new(crate::checkpoint::ResumePlan::fresh()),
            sqlite_reporter: sqlite_reporter.clone(),
            resource_pool: resource_pool.clone(),
            scene_tree_parent_id: 0,
            scene_tree_path: initial_scene_tree_path.clone(),
            current_scope_idx: 0,
        };

        // One Walker: seed the scope cursor at the scenario layer (the single
        // child of the workload root) so the top-level scenario nodes resolve
        // positionally against its children, not by AST match.
        exec_ctx.current_scope_idx = exec_ctx.scope_tree.scenario_root_idx();

        // Install empty SceneTree global; the walker populates it.
        crate::scene_tree::install_global(crate::scene_tree::SceneTree::new());

        // Pre-map structural pass. Errors propagate in strict mode
        // (SRD-15 §"Empty Iteration Sources"); otherwise the walker
        // logs and continues — downstream code handles the partial
        // / empty tree.
        let pre_map_result = crate::executor::execute_tree(&mut exec_ctx, &scenario_nodes).await;
        let pre_mapped_tree = match pre_map_result {
            Ok(()) => {
                let tree = crate::scene_tree::current();
                if let Some(ref t) = tree {
                    observer.scenario_pre_mapped(t);
                }
                tree
            }
            Err(e) if strict => return Err(e),
            Err(e) => {
                crate::diag!(
                    crate::observer::LogLevel::Warn,
                    "pre-map walker failed (scope hierarchy will be flat in summaries / TUI): {e}"
                );
                None
            }
        };

        // dryrun=kernels short-circuit: pre-map already ran;
        // the install-kernel visitor (registered above before
        // execute_tree) streamed each scope's polydat source
        // to stdout as the walk encountered it. Print the
        // legend + exit cleanly.
        if params.get("dryrun").map(|s| s.as_str()) == Some("kernels") {
            print_kernel_dump_legend();
            crate::scope_tree::set_kernel_install_visitor(None);
            return Ok(());
        }

        // --- Checkpoint writer + resume plan (SRD-44 / SRD-44a) ---
        //
        // The writer file lives at `<session-dir>/checkpoint.jsonl`
        // — an append-only JSONL event log per SRD-44a. Resume from
        // an explicit prior session is wired through the
        // `--resume <session>` / `--resume-latest` CLI surface
        // (see runner CLI parsing); for a fresh session the writer
        // starts empty and the plan reruns everything.
        // SRD-88 — the writer + resume doc are SESSION-tier (created
        // once in `SessionHost::setup`, holding the single resume lock).
        // This execution shares them; it derives its own resume plan
        // below from `saved_doc` + its pre-map.
        let checkpoint_writer = host.checkpoint_writer.clone();
        let saved_doc = host.saved_doc.clone();
        let invocation = saved_doc.as_ref().map(|d| d.invocation + 1).unwrap_or(1);

        // End-of-run notices: drops on success OR error path.
        //
        //  - Resume hint: when checkpoint state shows
        //    incomplete idempotent phases (SRD-44), advise the
        //    operator how to resume.
        //  - Keep-purge forecast: when the next new session
        //    would auto-purge sessions under the keep cap
        //    (SRD-45), let the operator know how many and how
        //    to disable.
        let parent_for_keep_check = if let Some(p) = session.output_dir.parent() {
            p.to_path_buf()
        } else {
            crate::session::default_sessions_root()
        };
        let session_keep = crate::session::resolve_session_dir(&args).session_keep;
        struct EndOfRunNoticeGuard {
            writer: std::sync::Arc<crate::checkpoint::CheckpointWriter>,
            parent: std::path::PathBuf,
            keep_cap: usize,
        }
        impl Drop for EndOfRunNoticeGuard {
            fn drop(&mut self) {
                if let Some(hint) = self.writer.resume_hint() {
                    // Through the observer, one line per log call —
                    // NOT a raw eprintln!: this drop can fire while
                    // the TUI still holds the terminal in raw mode,
                    // where a bare `\n` does not return the carriage
                    // and a stderr write bypasses the render sink —
                    // each line then starts at the previous line's
                    // end column (the end-of-session staircase,
                    // observed 2026-08-05). The log path is owned by
                    // the surface channel (SRD-87) in both TUI and
                    // plain modes, so it renders correctly in each.
                    for line in hint.lines() {
                        crate::diag!(crate::observer::LogLevel::Info, "{line}");
                    }
                }
                let n = crate::session::forecast_keep_purge(&self.parent, self.keep_cap);
                if n > 0 {
                    crate::diag!(
                        crate::observer::LogLevel::Info,
                        "the next new nmbrs session will auto-purge {n} prior session \
                         director{plural} under {} due to --session-keep={cap}. \
                         To disable: --session-keep=0 (or NMBRS_SESSION_KEEP=0). \
                         To raise the cap: --session-keep=<bigger>.",
                        self.parent.display(),
                        plural = if n == 1 { "y" } else { "ies" },
                        cap = self.keep_cap,
                    );
                }
            }
        }
        let _eor_notice_guard = EndOfRunNoticeGuard {
            writer: checkpoint_writer.clone(),
            parent: parent_for_keep_check,
            keep_cap: session_keep,
        };
        let resume_plan =
            if let (Some(saved), Some(tree)) = (saved_doc.as_ref(), pre_mapped_tree.as_ref()) {
                let candidates =
                    crate::checkpoint::scene_tree_resume_candidates(tree, &scope_tree, &phases);
                std::sync::Arc::new(crate::checkpoint::ResumePlan::from_checkpoint(
                    saved,
                    &candidates,
                    &workload_params,
                ))
            } else {
                std::sync::Arc::new(crate::checkpoint::ResumePlan::fresh())
            };

        // Declare every pre-mapped phase into the writer so a
        // future resume can tell "didn't run yet" from "wasn't
        // planned". Idempotent — re-declaring an entry the
        // writer already restored from disk is a no-op.
        if let Some(tree) = pre_mapped_tree.as_ref() {
            crate::checkpoint::declare_scene_tree_phases(&checkpoint_writer, tree, &phases);
        }

        if resume_plan.is_resume {
            let skip = resume_plan.skip_count();
            let mismatch = resume_plan.mismatch_count();
            let cursor = resume_plan.cursor_resume_count();
            crate::diag!(
                crate::observer::LogLevel::Info,
                "resume: invocation #{invocation} — \
                 {skip} skip, {mismatch} mismatched, {cursor} cursor-resume"
            );
        }

        // SRD-35 Push D: seed the resource pool's per-key
        // `pending_uses` counter before any phase runs. The
        // walker is a pure read of the pre-mapped tree +
        // session-level params; it doesn't instantiate any
        // adapter or open any resource. After this, the pool
        // can close `Shared`/`PerScenario` entries the moment
        // their last predicted phase detaches, instead of
        // holding them until session end.
        if let Some(tree) = pre_mapped_tree.as_ref() {
            crate::resource_pool::pre_map_pending_uses(
                &resource_pool,
                tree,
                &phases,
                &driver,
                &merged_params,
            )?;
        }

        // ─── Unified walker: execution pass ──────────────────────────
        //
        // Update the post-pre-map fields on the same `exec_ctx`
        // used for the pre-map pass: real `checkpoint_writer`,
        // resolved `resume_plan`, restored execution depth. Per
        // SRD 18b §"Single Walker Contract" point 1, this is the
        // SAME walker function — `execute_tree` — invoked again at
        // the configured depth. SceneTree::push is idempotent so
        // every node the pre-map pass pushed is reused.
        exec_ctx.checkpoint_writer = Some(checkpoint_writer.clone());
        exec_ctx.resume_plan = resume_plan.clone();
        exec_ctx.diag = diag.clone();
        exec_ctx.scene_tree_parent_id = 0;
        exec_ctx.scene_tree_path = initial_scene_tree_path.clone();
        // One Walker: seed at the scenario layer (see the pre-map seed above).
        exec_ctx.current_scope_idx = exec_ctx.scope_tree.scenario_root_idx();
        // Pre-map pass is done — the real execution starts now.
        // dryrun=phase still walks at depth=Phase but with this
        // flag false, so the sentinel set_phase_completed in the
        // walker fires as the dryrun=phase summary needs.
        exec_ctx.pre_map_only = false;

        let scheduler = crate::scheduler::build(&schedule_spec);
        let scheduler_result = scheduler.run(&mut exec_ctx, &scenario_nodes).await;

        // SRD-35: drain the resource pool at session end.
        // `Shared`/`PerScenario` entries intentionally stay alive
        // across phases (the whole reason the pool exists), so
        // this is the close trigger that releases their network
        // resources. Runs even if the scenario errored out —
        // half-open clusters would otherwise leak FDs into the
        // next session in TUI / `metrics watch` host processes.
        exec_ctx.resource_pool.shutdown().await;
        // SRD-93 stage 6 — a ladder-driven interrupt (level 1/2)
        // unwinds the walk as an `Err`, which the `?` below would
        // propagate PAST the SRD-77 row close-out at the end of this
        // function — leaving `disposition` NULL, the marker reserved
        // for genuinely unclean exits (force-exit, panic, SIGKILL). A
        // cooperative drain IS a clean shutdown: stamp the row with
        // the scene tree's computed disposition (interrupted / failed
        // / …) before the error propagates. `update_execution_end`
        // keys on `ended_at_nanos IS NULL`, so the normal-completion
        // stamp below stays a no-op after this one.
        if scheduler_result.is_err() {
            close_execution_row(&sqlite_reporter, &session_id, exec_id);
        }
        scheduler_result?;

        // Workload-end lifecycle boundary: every phase in the
        // scenario has completed. Individual phase paths already
        // closed themselves at phase-end, but any workload-level
        // ingests (e.g. aggregate metrics the tree code emits at
        // scope scope rather than phase scope) still need a flush.
        // In phased mode the workload's label set is the session
        // root — there's no intermediate `activity=...` label —
        // so we close at the session root.
        cadence_reporter.close_path(&Labels::of("session", &session.id));

        // SRD-13d Phase 7 — `dryrun=dispenser` scope-elision
        // summary. Phase walk has just completed; scope tree
        // carries final `materialised` / `logical_name` marks
        // (set by the workload-load classifier). Dump now so the
        // diagnostic surfaces phase-init artifacts (registered
        // metrics, adapter map_op calls) in the same run. Used
        // to fire for `Op` depth; since the auto-bump now lifts
        // `dryrun=op` to `Cycle` (full cycle execution with
        // wrapper short-circuit), the scope-elision surface
        // moved to `dryrun=dispenser` — the explicit "build
        // every dispenser but don't run cycles" mode.
        if diag.depth == ExecDepth::Dispenser {
            let mut out = std::io::stdout();
            if let Err(e) = render_scope_elision_summary(&scope_tree, &mut out) {
                crate::diag!(
                    crate::observer::LogLevel::Warn,
                    "warning: rendering scope-elision summary: {e}"
                );
            }
        }

        // The run reached its end boundary: every fallible step is
        // behind us (the epilogue after this block is teardown
        // only). Tell the checkpoint writer HERE, as the block's
        // last statement — `_eor_notice_guard` drops when this
        // block closes, and its resume_hint must read
        // declared-but-never-started phases as excluded-by-
        // predicate rather than left-behind (SRD-44a; see
        // resume_hint). Error `?` returns above skip this, so a
        // cut-short run still advises resuming pending phases.
        checkpoint_writer.mark_run_reached_end();
    }

    // Session-end lifecycle boundary. Close the session root path
    // for any aggregate windows that were ingested at session level
    // (rare today, but the boundary must be explicit — otherwise
    // session-level aggregates would only flush during
    // `shutdown_flush` at the very end, after all the per-subscriber
    // teardown logic had already started).
    cadence_reporter.close_path(&Labels::of("session", &session.id));

    // SRD-88 — flush THIS execution's windows into the store so the
    // summaries below see complete data, without tearing down the
    // session-shared cadence reporter.
    cadence_reporter
        .quiesce(std::time::Duration::from_secs(30))
        .await;

    // SRD-63 Push 9a: fire `EventType::SessionEnd` once after
    // the cadence shutdown but before `run_finished()`.
    // Both branches (phased + single-activity) converge
    // here, so a single fire covers every run shape.
    {
        let session_ctx = crate::readout_context::LifecycleContext {
            event: crate::lifecycle::EventType::SessionEnd,
            subject_name: session.id.clone(),
            subject_labels: String::new(),
            depth_indent: String::new(),
            use_color: crate::observer::use_color(),
            stick_reattached: String::new(),
        };
        crate::readout_context::fire_lifecycle(
            crate::lifecycle::EventType::SessionEnd,
            &workload_readouts,
            None,
            &session_ctx,
            Some(&sqlite_reporter),
        );
    }

    observer.run_finished();

    if dry_run.is_some() {
        crate::diag!(crate::observer::LogLevel::Info, "dry-run complete.");
    } else {
        crate::diag!(crate::observer::LogLevel::Info, "done.");
    }

    // Build the active set of named summaries.
    //
    // Precedence:
    //   - CLI `summary=<spec>` wins outright — produces a single
    //     ad-hoc summary under the synthetic name `default`,
    //     overriding any workload-declared map. Matches prior
    //     CLI behavior.
    //   - Otherwise the workload's `summary:` map (and the
    //     `summary.yaml` sidecar fallback already merged into
    //     `workload_summaries` above) is used as-is.
    //
    // An empty map means "no summary at end of run" — same as
    // the legacy "no `summary:` field" case.
    let active_summaries: HashMap<String, nmbrs_workload::model::SummaryConfig> =
        if let Some(cli_summary) = merged_params.get("summary") {
            let mut m = HashMap::new();
            m.insert(
                "default".into(),
                nmbrs_workload::model::SummaryConfig::parse(cli_summary),
            );
            m
        } else {
            workload_summaries.clone()
        };

    // SRD-46 output routing for the in-run summary, by item name.
    //
    // A CLI `summary=<spec>` is an explicit operator request —
    // they typed it, so it still echoes to the terminal. A
    // workload-declared table goes where its `to:` says, and one
    // that declared nothing goes to the session directory ONLY.
    // stdout is never implied for automatic rendering: a run's
    // stdout is its op output, and a summary carries wall-clock
    // values that would make that output differ run to run.
    let summary_destinations: HashMap<String, Vec<nmbrs_workload::report::Destination>> = {
        use nmbrs_workload::report::Destination as D;
        if merged_params.contains_key("summary") {
            let mut m = HashMap::new();
            m.insert("default".to_string(), vec![D::SessionDir, D::Stdout]);
            m
        } else {
            workload_report
                .items()
                .filter(|i| matches!(i.kind, nmbrs_workload::report::Kind::Table))
                .map(|i| {
                    (
                        i.name.clone(),
                        i.style
                            .destinations
                            .clone()
                            .unwrap_or_else(|| vec![D::SessionDir]),
                    )
                })
                .collect()
        }
    };

    // SRD-46 Details auto-injection: persist run-wide context
    // (end time, phase + scenario counts, adapter, …) into
    // session_metadata regardless of whether the workload
    // declared a `report:` block. Post-run hooks read this to
    // build the auto-injected Details section that lands at
    // the top of every output markdown file.
    if let Ok(mut guard) = sqlite_reporter.lock()
        && let Some(ref mut reporter) = *guard
    {
        let end_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let sid = session.id.clone();
        let exec_id = execution.exec_id;
        reporter.set_execution_metadata(&sid, exec_id, "end_time", &end_time.to_string());
        reporter.set_execution_metadata(&sid, exec_id, "phase_count", &phases.len().to_string());
        reporter.set_execution_metadata(
            &sid,
            exec_id,
            "scenario_count",
            &scenarios.len().to_string(),
        );
        if let Some(wf) = workload_file.as_deref() {
            reporter.set_execution_metadata(&sid, exec_id, "workload_file", wf);
        }
        reporter.set_execution_metadata(&sid, exec_id, "adapter", &driver);
    }

    if !active_summaries.is_empty() {
        // Summary report always comes from SQLite — the
        // durable record. The in-memory store exists for GK
        // access and reactive control, not for reporting.
        if let Ok(mut guard) = sqlite_reporter.lock()
            && let Some(ref mut reporter) = *guard
        {
            // Persist every report item (SRD-46) under
            // `report.<name>` keys. Value carries the kind
            // keyword (`plot ...` / `table ...`) followed
            // by an optional `label "..."` line and then
            // the spec body — same shape the report parser
            // ingests, so the db-fallback path in
            // `nmbrs report` round-trips through the same
            // parser the workload uses.
            let sid = session.id.clone();
            let exec_id = execution.exec_id;
            for item in workload_report.items() {
                // Single emission point: the workload-side
                // serializer. The db-fallback path in
                // `nmbrs report` parses this value back
                // through `parse_persisted_item`, which
                // uses the same grammar — round-trip safe.
                let value = item.to_yaml_directive_string();
                reporter.set_execution_metadata(
                    &sid,
                    exec_id,
                    &format!("report.{}", item.name),
                    &value,
                );
            }

            // Stable ordering for consistent output across
            // runs (HashMap iteration is non-deterministic).
            let mut names: Vec<&String> = active_summaries.keys().collect();
            names.sort();
            for name in names {
                let cfg = &active_summaries[name];
                let (basename, format) =
                    nmbrs_metrics::reporters::sqlite::derive_name_and_format(name);
                // SRD-77 — the in-run summary is naturally
                // scoped to the current execution; qualifier
                // narrows to this run's exec_id so a refine
                // doesn't render rows from prior runs.
                let report_config = report_config_from_summary(cfg, Some(exec_id));
                let rendered = reporter.format_summary_with_format(&report_config, &format);
                if rendered.is_empty() {
                    continue;
                }
                // SRD-46 routing for this item. Undeclared ⇒
                // session directory only.
                let dests = summary_destinations
                    .get(name.as_str())
                    .cloned()
                    .unwrap_or_else(|| vec![nmbrs_workload::report::Destination::SessionDir]);
                let to_session = dests.contains(&nmbrs_workload::report::Destination::SessionDir);
                let to_stdout = dests.contains(&nmbrs_workload::report::Destination::Stdout);
                let to_stderr = dests.contains(&nmbrs_workload::report::Destination::Stderr);

                let filename = format!("{basename}_summary.{format}");
                let summary_path = session.output_dir.join(&filename);
                if to_session {
                    if let Err(e) = std::fs::write(&summary_path, &rendered) {
                        crate::diag!(
                            crate::observer::LogLevel::Warn,
                            "warning: failed to write summary to {}: {e}",
                            summary_path.display()
                        );
                    } else {
                        crate::diag!(
                            crate::observer::LogLevel::Info,
                            "summary: {}",
                            summary_path.display()
                        );
                    }
                }
                // Inline print only when the observer is
                // not suppressing stderr — i.e. we're in
                // tui=off mode and the user can see stdout
                // right now. In TUI mode the alternate
                // screen is up, so `print!()` here would
                // get buffered behind the TUI rendering and
                // discarded on teardown. The persona reads
                // the *_summary.* files and prints them
                // post-shutdown (see `nmbrs/src/run.rs`).
                if to_stderr {
                    eprint!("{rendered}");
                }
                if to_stdout {
                    // In TUI mode the alternate screen is up, so an
                    // inline `print!` would be buffered behind the
                    // TUI rendering and discarded on teardown.
                    // Defer it to a file the post-run printer
                    // flushes once the terminal is back in cooked
                    // mode. Routing is decided HERE either way —
                    // the post-run printer no longer infers "goes
                    // to stdout" from the presence of an artifact.
                    if observer.suppresses_stderr() {
                        let deferred = session.output_dir.join(DEFERRED_STDOUT_FILE);
                        if let Err(e) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&deferred)
                            .and_then(|mut f| {
                                std::io::Write::write_all(&mut f, rendered.as_bytes())
                            })
                        {
                            crate::diag!(
                                crate::observer::LogLevel::Warn,
                                "warning: failed to defer summary stdout to {}: {e}",
                                deferred.display()
                            );
                        }
                    } else {
                        print!("{rendered}");
                    }
                }
            }
        }
    }

    // Refresh convenience symlinks at the logs/ root so
    //   logs/metrics.db, logs/summary.md, logs/session.log
    // always resolve to this session's artifacts. `logs/latest` (a
    // symlink to the whole session dir) is created by Session::new;
    // these are per-file counterparts for direct tool access like
    // `sqlite3 logs/metrics.db` or `tail -f logs/session.log`.
    refresh_latest_file_links(&session);

    // dryrun=controls: every phase has now been constructed (at
    // depth=Phase the executor stops before cycles but still
    // attaches components and declares controls). Walk the
    // session tree and dump the catalog.
    if diag.list_controls {
        let mut out = std::io::stdout();
        if let Err(e) = render_controls_tree(&session.component, &mut out) {
            crate::diag!(
                crate::observer::LogLevel::Warn,
                "warning: rendering controls: {e}"
            );
        }
    }

    // SRD-77 / SRD-93 stage 6 — close out the executions row with
    // the computed disposition + end timestamp. Both clean-exit
    // paths stamp it: normal completion here, and a ladder-driven
    // interrupt at the walk-error site above (which then propagates
    // out before reaching this line). Only genuinely unclean exits
    // (force-exit, panic, SIGKILL) leave `ended_at_nanos` NULL,
    // which the read side surfaces as "execution in flight /
    // unclean exit" distinct from a recorded outcome.
    close_execution_row(&sqlite_reporter, &session_id, exec_id);

    // WAL consolidation runs from the RAII shutdown guard
    // bound at the top of this function (`_sqlite_shutdown_guard`).
    // The guard's Drop fires reliably across normal return,
    // `?` error propagation, and first-Ctrl-C cooperative
    // shutdown — every path Rust unwinds through. Second
    // Ctrl-C → `process::exit` is the only skip path,
    // matching the documented force-exit semantic in
    // `session_signals`.

    Ok(())
}

/// SRD-77 / SRD-93 stage 6 — close the in-flight executions row with
/// the scene tree's session disposition + an end timestamp. Idempotent
/// (`update_execution_end` keys on `ended_at_nanos IS NULL`), so the
/// interrupt-path caller and the normal-completion caller compose:
/// whichever runs first wins, the other is a no-op.
fn close_execution_row(
    sqlite_reporter: &std::sync::Arc<
        std::sync::Mutex<Option<nmbrs_metrics::reporters::sqlite::SqliteReporter>>,
    >,
    session_id: &str,
    exec_id: u64,
) {
    let disposition =
        crate::scene_tree::with_global(|t| t.session_disposition().label()).unwrap_or("UNKNOWN");
    let ended_at_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    if let Ok(mut g) = sqlite_reporter.lock()
        && let Some(r) = g.as_mut()
    {
        r.update_execution_end(session_id, exec_id, ended_at_nanos, disposition);
    }
}

/// Core runner: set up the shared session host, run one execution, tear down.
async fn run_impl(
    args: &[String],
    observer: Arc<dyn crate::observer::RunObserver>,
) -> Result<(), String> {
    let host = SessionHost::setup(args, observer.clone())?;
    let result = run_execution(&host, args, observer).await;
    host.shutdown().await;
    result
}

/// SRD-88 — one execution's spec for [`run_executions`]: the workload
/// CLI args, the observer that captures its lifecycle/log, and an optional
/// per-execution output channel (SRD-87 buckets). `channel = None` falls back
/// to the process-global channel; in-process example verification passes a
/// `CaptureChannel` so each execution's op stdout is captured separately.
pub struct ExecutionSpec {
    pub args: Vec<String>,
    pub observer: Arc<dyn crate::observer::RunObserver>,
    pub channel: Option<Arc<dyn crate::output_channel::OutputChannel>>,
}

/// SRD-88 — run N executions CONCURRENTLY in one process, all sharing
/// ONE session, at most `max_concurrent` in flight. The session
/// (`SessionHost`: dir / stores / cadence + scheduler services) is set
/// up ONCE and torn down ONCE, after every execution. Each execution
/// loads + runs its own workload, derives its own `Execution`
/// (distinct allocated `exec_id`) under the shared session component,
/// flushes its metrics into the shared store via
/// [`CadenceReporter::quiesce`](nmbrs_metrics::cadence_reporter::CadenceReporter::quiesce)
/// without tearing the reporter down, and routes its lifecycle/log
/// through its own observer (a scoped [`ExecutionContext`]). Results
/// come back in input order.
///
/// `max_concurrent == 1` is the sequential case — the SAME harness, no
/// separate path (SRD-02 "One Concurrency Path").
pub async fn run_executions(
    session_args: &[String],
    session_observer: Arc<dyn crate::observer::RunObserver>,
    specs: Vec<ExecutionSpec>,
    max_concurrent: usize,
) -> Result<Vec<Result<(), String>>, String> {
    let host = std::sync::Arc::new(SessionHost::setup(session_args, session_observer)?);
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrent.max(1)));
    let futs = specs.into_iter().map(|spec| {
        let host = host.clone();
        let sem = sem.clone();
        async move {
            // Bound in-flight executions; permit held for the whole run.
            let _permit = sem.acquire().await.expect("semaphore not closed");
            // Scope this execution's context — a distinct allocated
            // `exec_id` + its own observer (+ optional output channel) — so
            // deeply-nested fibers, op-output routing, and `run_execution`'s
            // exec-identity all resolve to THIS execution.
            let ctx = match spec.channel.clone() {
                Some(ch) => crate::execution_context::ExecutionContext::with_observer_and_channel(
                    spec.observer.clone(),
                    ch,
                ),
                None => {
                    crate::execution_context::ExecutionContext::with_observer(spec.observer.clone())
                }
            };
            crate::execution_context::scope(ctx, run_execution(&host, &spec.args, spec.observer))
                .await
        }
    });
    let results = futures::future::join_all(futs).await;
    // Session teardown — once, after every execution completed (so the
    // host is now the sole owner).
    match std::sync::Arc::try_unwrap(host) {
        Ok(h) => h.shutdown().await,
        Err(_) => crate::diag!(
            crate::observer::LogLevel::Warn,
            "run_executions: session host still referenced at teardown; \
             scheduler/WAL will close on drop"
        ),
    }
    Ok(results)
}

/// Point per-file symlinks under `logs/` at the latest session's
/// artifacts. Silently skips files that don't exist (e.g. summary.md
/// when no `summary:` was declared). Replaces any existing symlink.
///
/// Targets route through `logs/latest` (which `Session::new` points
/// at the actual session dir) so the convenience links stay
/// consistent with `latest`. Skipped entirely when the session
/// lives outside `logs/` — `--session-path /tmp/x` is an explicit
/// redirect and shouldn't hijack the user's `logs/` symlinks.
fn refresh_latest_file_links(session: &crate::session::Session) {
    let logs_dir = std::path::Path::new("logs");
    // Mirror the gate in `Session::new` — keep these convenience
    // links and `logs/latest` synchronized: either both update or
    // neither does.
    if !crate::session::target_is_under(logs_dir, &session.output_dir) {
        return;
    }
    for file in ["metrics.db", "summary.md", "session.log"] {
        let target = session.output_dir.join(file);
        if !target.exists() {
            continue;
        }
        let link = logs_dir.join(file);
        // Remove any existing entry (symlink or regular file) so we can
        // recreate the link. If this fails because nothing's there,
        // that's fine.
        let _ = std::fs::remove_file(&link);
        let rel_target = std::path::Path::new("latest").join(file);
        if let Err(e) = crate::session::symlink_any(&rel_target, &link) {
            crate::diag!(
                crate::observer::LogLevel::Warn,
                "warning: failed to link {} → {}: {e}",
                link.display(),
                rel_target.display()
            );
        }
    }
}

/// Create an adapter from inventory registrations.
///
/// `dryrun=cycle` does NOT substitute the adapter here — it means
/// "construct a fully-executable cycle path, then suppress only
/// the outbound `execute()` at cycle time." The real adapter is
/// always created (connecting, preparing statements, gathering
/// metadata); the outermost `DryRunWrapper` handles the runtime
/// short-circuit. See `nmbrs_runtime::wrappers::DryRunWrapper`.
pub async fn create_adapter(
    driver: &str,
    params: &HashMap<String, String>,
) -> Result<Arc<dyn crate::adapter::DriverAdapter>, String> {
    let reg = find_adapter_registration(driver).ok_or_else(|| {
        let available = registered_driver_names();
        format!(
            "unknown adapter '{driver}' (available: {})",
            available.join(", ")
        )
    })?;
    (reg.create)(params.clone()).await
}

/// Run an activity without its own capture thread.
///
/// All metrics flow through the session-level scheduler →
/// `CadenceReporter` → `MetricsQuery`. This function just runs the
/// activity to completion; lifecycle flush (final delta +
/// validation metrics) is handled by the caller (executor).
/// Streaming print: one header line for the `dryrun=kernels`
/// dump, fired before any kernel installs.
fn print_kernel_dump_header() {
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let (bold, reset) = if is_tty {
        ("\x1b[1m", "\x1b[0m")
    } else {
        ("", "")
    };
    println!();
    println!("{bold}Polydat Scope Kernels{reset}");
    println!("{bold}═════════════════════{reset}");
    println!();
}

/// Streaming per-scope visitor callback. Prints the scope's
/// logical name + the polydat source the kernel was compiled
/// from, indented by the scope's depth.
fn print_kernel_for_scope(
    node: &crate::scope_tree::ScopeNode,
    kernel: &polydat::kernel::PolydatKernel,
) {
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let (bold, dim, reset, cyan, magenta, green) = if is_tty {
        (
            "\x1b[1m", "\x1b[2m", "\x1b[0m", "\x1b[36m", "\x1b[35m", "\x1b[32m",
        )
    } else {
        ("", "", "", "", "", "")
    };

    let logical = if node.logical_name.is_empty() {
        "<unnamed scope>".to_string()
    } else {
        node.logical_name.clone()
    };
    let depth_indent = " ".repeat(node.depth);
    println!(
        "{depth_indent}{green}●{reset} {bold}{cyan}{logical}{reset} \
              {dim}(depth={}, kind={:?}){reset}",
        node.depth, node.kind
    );
    let source = kernel.program().source().trim_end();
    if source.is_empty() {
        println!("{depth_indent}  {dim}(empty kernel — no own bindings){reset}");
    } else {
        for line in source.lines() {
            println!("{depth_indent}  {magenta}│{reset} {line}");
        }
    }
    println!();
}

/// Footer for the `dryrun=kernels` dump (legend + spacing).
fn print_kernel_dump_legend() {
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let (dim, reset, green) = if is_tty {
        ("\x1b[2m", "\x1b[0m", "\x1b[32m")
    } else {
        ("", "", "")
    };
    println!(
        "  {dim}Legend: {green}●{reset}{dim} kernel installed at this scope. \
              Flattened scopes (those that inherit a parent's kernel) emit no entry.{reset}"
    );
    println!();
}

pub async fn run_activity_simple(
    activity: Activity,
    adapters: std::collections::HashMap<String, Arc<dyn crate::adapter::DriverAdapter>>,
    default_adapter: &str,
    op_builder: Arc<crate::synthesis::OpBuilder>,
) -> bool {
    activity
        .run_with_adapters(adapters, default_adapter, op_builder)
        .await
}

/// Adapter that delegates to an `Arc<Mutex<Option<SqliteReporter>>>`.
///
/// Allows the SQLite reporter to be registered on the scheduler while
/// also being accessible for summary queries after the scheduler stops.
struct MutexReporter(
    std::sync::Arc<std::sync::Mutex<Option<nmbrs_metrics::reporters::sqlite::SqliteReporter>>>,
);

impl Reporter for MutexReporter {
    fn report(&mut self, snapshot: &nmbrs_metrics::snapshot::MetricSet) {
        if let Ok(mut guard) = self.0.lock()
            && let Some(ref mut r) = *guard
        {
            Reporter::report(r, snapshot);
        }
    }

    fn flush(&mut self) {
        if let Ok(mut guard) = self.0.lock()
            && let Some(ref mut r) = *guard
        {
            Reporter::flush(r);
        }
    }
}

/// Wrapper to make `Box<dyn Reporter>` usable with `add_reporter(impl Reporter)`.
struct BoxedReporter(Box<dyn Reporter>);
impl Reporter for BoxedReporter {
    fn report(&mut self, snapshot: &nmbrs_metrics::snapshot::MetricSet) {
        self.0.report(snapshot);
    }
    fn flush(&mut self) {
        self.0.flush();
    }
}

// =========================================================================
// Helpers
// =========================================================================

/// Expand `{key}` workload param placeholders in a string.
pub fn expand_workload_params(s: &str, params: &HashMap<String, String>) -> String {
    let mut result = s.to_string();
    for (key, value) in params {
        let placeholder = format!("{{{key}}}");
        if result.contains(&placeholder) {
            result = result.replace(&placeholder, value);
        }
    }
    result
}

/// Collected param references from a workload, separating direct
/// `{name}` references from composite-name templates like
/// `{k_{k}_limits}` whose ground form depends on a runtime
/// substitution.
///
/// Used by the workload param validator to recognize that a
/// declared param like `k_1_limits` is genuinely referenced
/// when the workload uses `{k_{k}_limits}` and `k` ranges over
/// values including `1`.
#[derive(Default)]
struct ParamRefs {
    /// Names that appeared as `{name}` curly-brace placeholders.
    /// These MUST resolve to a declared param, runner-known
    /// param, adapter-registered param, or scenario-tree
    /// iter-var — anything else is a typo or missing
    /// declaration and the validator surfaces it as an error.
    placeholders: std::collections::HashSet<String>,
    /// Placeholders that appeared in scenario-tree `for_each` /
    /// `for_combinations` / DoWhile-condition text. These get
    /// resolved at runtime by the comprehension interpolator
    /// (which emits a `<path>:<line>:<col>:` error on failure);
    /// the strict "must-resolve-now" validator skips them so the
    /// more-specific runtime diagnostic wins. They still count
    /// as references for the "declared but unreferenced" check
    /// — a workload param used only in a `for_each` spec is
    /// genuinely consumed by the workload, just at runtime.
    runtime_only_placeholders: std::collections::HashSet<String>,
    /// Bare identifiers harvested from `if:` / `delay:`
    /// expression bodies. These may be wire names (referencing
    /// values bound via Polydat source) rather than workload params,
    /// so they participate in the "declared but unreferenced"
    /// check (as references) but NOT in the "referenced but
    /// undeclared" check (since wire names legitimately live
    /// outside the workload's param surface).
    expression_idents: std::collections::HashSet<String>,
    /// Composite templates: the literal body of a `{...}` whose
    /// inner content contained nested `{...}`. Stored verbatim
    /// (e.g. `"k_{k}_limits"`); validation checks each declared
    /// param name against these templates by replacing each
    /// inner `{NAME}` with a word-character wildcard.
    templates: Vec<String>,
}

impl ParamRefs {
    /// Does `param` appear as a reference anywhere — placeholder,
    /// runtime-only placeholder, expression ident, or composite
    /// template? Used by the existing "declared but unreferenced"
    /// validator.
    fn contains(&self, param: &str) -> bool {
        if self.placeholders.contains(param) {
            return true;
        }
        if self.runtime_only_placeholders.contains(param) {
            return true;
        }
        if self.expression_idents.contains(param) {
            return true;
        }
        self.templates
            .iter()
            .any(|tpl| template_matches(tpl, param))
    }
}

/// Match a declared param name against a composite-name template.
///
/// `template` is the body of a `{...}` reference whose content
/// included nested `{...}` substitutions — e.g. `k_{k}_limits`.
/// Each inner `{NAME}` matches one or more word characters
/// (`[A-Za-z0-9_]+`); the surrounding literal chars must match
/// exactly. Returns `true` iff `param` exactly matches the
/// template's ground form for some substitution of the inner
/// names.
fn template_matches(template: &str, param: &str) -> bool {
    let t = template.as_bytes();
    let p = param.as_bytes();
    let mut ti = 0;
    let mut pi = 0;
    while ti < t.len() {
        if t[ti] == b'{' {
            // Skip past the inner {...}. The template body
            // doesn't nest deeper than one level in practice
            // (composed names like `{k_{k}_limits}` don't
            // contain `{a_{b}_c}` recursively); a simple
            // first-`}` lookup suffices.
            let close = match template[ti + 1..].find('}') {
                Some(n) => ti + 1 + n,
                None => return false, // malformed template
            };
            // Determine where the template's literal context
            // resumes after the inner placeholder.
            let next_lit = close + 1;
            // The next literal char (or end-of-template) bounds
            // how far the wildcard can consume.
            if next_lit >= t.len() {
                // Wildcard must consume the rest of `param`,
                // and that suffix must be at least one word char.
                if pi >= p.len() {
                    return false;
                }
                return p[pi..]
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || *b == b'_');
            }
            let stop = t[next_lit];
            // Greedy-match word chars in param up to the next
            // literal in template.
            let mut consumed = 0;
            while pi + consumed < p.len() && p[pi + consumed] != stop {
                let b = p[pi + consumed];
                if !(b.is_ascii_alphanumeric() || b == b'_') {
                    return false;
                }
                consumed += 1;
            }
            if consumed == 0 {
                return false;
            } // wildcard requires ≥1 char
            pi += consumed;
            ti = next_lit;
        } else {
            // Literal char: must match.
            if pi >= p.len() || p[pi] != t[ti] {
                return false;
            }
            ti += 1;
            pi += 1;
        }
    }
    pi == p.len()
}

/// Scan a string for `{name}` references and `{composite_{x}_name}`
/// templates, accumulating into `refs`.
///
/// Plain `{name}` placeholders (where the body is a single
/// identifier — alphanumerics + underscore, leading non-digit)
/// are recorded as direct references. A `{...}` whose body
/// contains nested `{...}` is recorded as a template; the inner
/// leaf names are also recorded as direct references because
/// they're the substitution inputs (e.g. `{k_{k}_limits}`
/// records the template `k_{k}_limits` AND the direct ref `k`).
/// Walk a `serde_json::Value` and call [`scan_param_refs`] on
/// every string leaf. Used by [`collect_param_references`] to
/// reach `{name}` references nested inside structured
/// `params:` blocks (e.g. `relevancy: { expected: "{ground_truth}" }`).
fn scan_json_for_refs(v: &serde_json::Value, refs: &mut ParamRefs) {
    match v {
        serde_json::Value::String(s) => scan_param_refs(s, refs),
        serde_json::Value::Array(a) => {
            for item in a {
                scan_json_for_refs(item, refs);
            }
        }
        serde_json::Value::Object(m) => {
            for item in m.values() {
                scan_json_for_refs(item, refs);
            }
        }
        _ => {} // numbers, booleans, null — no string content
    }
}

fn scan_param_refs(text: &str, refs: &mut ParamRefs) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        // Find the matching `}`, balancing nested `{`s.
        let body_start = i + 1;
        let mut depth = 1;
        let mut j = body_start;
        while j < bytes.len() && depth > 0 {
            match bytes[j] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                break;
            }
            j += 1;
        }
        if depth != 0 {
            // Unmatched `{` — bail, treat as literal.
            break;
        }
        let body = &text[body_start..j];
        if body.contains('{') {
            // Composite template (e.g. `k_{k}_limits`).
            refs.templates.push(body.to_string());
            // Recurse into the body to pick up the inner leaf
            // names as direct references.
            scan_param_refs(body, refs);
        } else if !body.is_empty()
            && body.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && !body.bytes().next().unwrap().is_ascii_digit()
        {
            // Plain `{name}` — curly-brace placeholder. MUST
            // resolve to a declared / known / iter-var name;
            // the new validator surfaces an error if it doesn't.
            refs.placeholders.insert(body.to_string());
        } else {
            // Inline Polydat expression body, e.g.
            // `{is_one_of(cassandra_dialect, "cndb")}` or
            // `{:=mod(hash(cycle), 100):=}`. Walk the body,
            // collect identifier-shaped tokens that aren't
            // inside string literals — those are Polydat name
            // references which may resolve to workload params.
            // Over-collecting (function names, Polydat stdlib
            // identifiers) is harmless — the validator's
            // membership test below uses the workload's own
            // declared params as the universe of interest.
            //
            // These land in `expression_idents` (not
            // `placeholders`) because they may legitimately
            // resolve to Polydat wire names rather than workload
            // params; the "declared but unreferenced" check
            // still consults them via `ParamRefs::contains`,
            // but the new "referenced but undeclared" check
            // only scrutinises `placeholders`.
            scan_expression_idents(body, &mut refs.expression_idents);
        }
        i = j + 1;
    }
}

/// Walk a GK-expression body (no surrounding `{}`) and add any
/// identifier-shaped tokens to `out`. Skips identifiers nested
/// inside `"..."` or `'...'` string literals — those are CQL /
/// regex / display strings, not name references. Recognises
/// backslash escapes inside string literals.
///
/// This is best-effort: it doesn't honor Polydat lexer subtleties
/// (numeric suffixes, raw strings, etc.). For the unused-param
/// check in `runner.rs::collect_param_references` the goal is
/// "does the param name appear anywhere we'd evaluate it" — a
/// loose match is correct because false positives only mean
/// "param is considered used when it might not have been",
/// which is the safer failure mode.
fn scan_expression_idents(body: &str, out: &mut std::collections::HashSet<String>) {
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // Skip `//` line comments — checked BEFORE the string-literal
        // scan below, because an apostrophe in comment prose (`hasn't`,
        // `don't`) would otherwise be read as an unterminated string
        // delimiter and swallow every identifier to end-of-input,
        // falsely flagging a later-referenced param as unused.
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Skip string literals.
        if b == b'"' || b == b'\'' {
            let quote = b;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        // Identifier start: ASCII letter or underscore.
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &body[start..i];
            // Skip the few literals the Polydat lexer also recognises
            // — they're definitely not param names.
            if ident != "true" && ident != "false" {
                out.insert(ident.to_string());
            }
            continue;
        }
        i += 1;
    }
}

/// Collect all `{name}` param references from a workload's ops,
/// phases, bindings, and scenario tree. Returns both direct
/// refs and composite templates so the validator can recognize
/// dynamic-name-composition references like `{k_{k}_limits}`.
fn collect_param_references(workload: &nmbrs_workload::model::Workload) -> ParamRefs {
    let mut refs = ParamRefs::default();

    // Local helper so every op-bearing scope (top-level + per-
    // phase) hits the same set of fields. Critically this
    // includes the *core* fields the parser hoists out of the
    // op map — `if:` (condition), `delay:`, and the
    // serde_json values inside `params:`. Missing any of those
    // produced false positives on the unused-param check
    // (e.g. `if: '{is_one_of(cassandra_dialect, "cndb")}'`
    // landed in `condition` rather than `op.op`, so the
    // workload param `cassandra_dialect` looked unreferenced).
    fn scan_op(op: &nmbrs_workload::model::ParsedOp, refs: &mut ParamRefs) {
        for value in op.op.values() {
            if let serde_json::Value::String(s) = value {
                scan_param_refs(s, refs);
            }
        }
        // `if:` and `delay:` accept either `{name}` placeholders
        // (caught by `scan_param_refs`) or bare wire names like
        // `delay: think_time`. Walk both shapes — the bare-ident
        // pass mirrors what `scope.rs` Step 3/6 do for the same
        // fields, so a workload param consumed only via a bare
        // delay/condition reference doesn't trip the unused-param
        // validator.
        if let Some(s) = &op.condition {
            scan_param_refs(s, refs);
            scan_expression_idents(s, &mut refs.expression_idents);
        }
        if let Some(spec) = &op.delay {
            for name in spec.names() {
                scan_param_refs(name, refs);
                scan_expression_idents(name, &mut refs.expression_idents);
            }
        }
        // `params:` values can be strings, numbers, nested
        // maps (e.g. `relevancy: { actual: key, expected: …}`).
        // Walk the JSON recursively so anything stringy gets
        // scanned regardless of nesting depth.
        //
        // `gutter:` is exempt: its templates resolve at RUNTIME
        // (wires first, then status-metric aggregates like
        // `{recall}` / `{latency_p50}` which have no workload
        // declaration), and an unresolved name degrades to
        // visible literal text in the cell — never a silent
        // failure this validator needs to preempt.
        for (k, v) in op.params.iter() {
            if k == "gutter" {
                continue;
            }
            scan_json_for_refs(v, refs);
        }
        // Evaluation blocks reference WIRES by bare identifier
        // (`relevancy: { k: suite_k, expected: ground_truth }`) —
        // the same shapes `scope.rs` resolves at wrap-time
        // through the op-template kernel. Harvest them as
        // expression idents so a param consumed only as an
        // evaluation bound doesn't trip the unused-param check.
        // (Before this, such workloads only validated by ACCIDENT:
        // any `{{…}}` inline expression elsewhere in the merged
        // doc registered a composite template that wildcard-
        // matched every declared param.)
        if let Some(rel) = op.params.get("relevancy").and_then(|v| v.as_object()) {
            for key in ["actual", "expected", "k", "r"] {
                if let Some(s) = rel.get(key).and_then(|v| v.as_str()) {
                    scan_expression_idents(s, &mut refs.expression_idents);
                }
            }
        }
        match &op.bindings {
            nmbrs_workload::model::BindingsDef::PolydatSource(s) => {
                // Two reference shapes inside Polydat source:
                //   - `{name}` placeholders inside string literals
                //     (resolved by Polydat string-interpolation against
                //     `const` bindings),
                //   - bare identifier references in Polydat expressions
                //     (e.g. `row := char_buf(..., cols)` — `cols`
                //     resolves directly to its `const` binding).
                // The unused-param validator must recognise both
                // or it falsely flags params that the workload
                // legitimately consumes via the bare path.
                scan_param_refs(s, refs);
                scan_expression_idents(s, &mut refs.expression_idents);
            }
            nmbrs_workload::model::BindingsDef::Map(m) => {
                for v in m.values() {
                    scan_param_refs(v, refs);
                }
            }
        }
    }

    // Scan top-level ops
    for op in &workload.ops {
        scan_op(op, &mut refs);
    }

    // SRD-13f Push D: workload-level `bindings:` are no longer
    // folded into ops at YAML parse time — they live on
    // `workload.bindings` and reach descendants via the GK
    // kernel chain. The unused-param validator must scan them
    // directly here, otherwise a workload param consumed only
    // from workload-level bindings (`row := char_buf(..., cols)`)
    // looks unreferenced.
    match &workload.bindings {
        nmbrs_workload::model::BindingsDef::PolydatSource(s) => {
            scan_param_refs(s, &mut refs);
            scan_expression_idents(s, &mut refs.expression_idents);
        }
        nmbrs_workload::model::BindingsDef::Map(m) => {
            for v in m.values() {
                scan_param_refs(v, &mut refs);
            }
        }
    }

    // SRD-83 — workload-level `stop_when:` predicates: `{param}` interpolation
    // only (same as phase-level), so a param used only by a workload breaker
    // counts as referenced.
    for c in &workload.stop_when {
        scan_param_refs(&c.when, &mut refs);
    }

    // Scan phases
    for phase in workload.phases.values() {
        if let Some(s) = &phase.cycles {
            scan_param_refs(s, &mut refs);
        }
        // SRD-83 (C3) governance `timeout:` and SRD-75 `interval:`
        // resolve `{param}` at phase setup — documented reference
        // sites the collector must count (before this, workloads
        // using them only validated via the accidental composite-
        // template wildcard).
        if let Some(s) = &phase.timeout {
            scan_param_refs(s, &mut refs);
        }
        if let Some(s) = &phase.interval {
            scan_param_refs(s, &mut refs);
        }
        if let Some(s) = &phase.concurrency {
            scan_param_refs(s, &mut refs);
        }
        if let Some(s) = &phase.for_each {
            scan_param_refs(s, &mut refs);
        }
        // Phase `rate:` and the SRD-75 poll bounds carry `{param}`
        // references resolved at the phase gather (the `timeout:`
        // discipline) — documented reference sites the collector
        // must count. The poll predicate additionally consumes
        // bare identifiers (params/wires) like `continue_if`.
        if let Some(s) = &phase.rate {
            scan_param_refs(s, &mut refs);
        }
        if let Some(poll) = &phase.poll {
            scan_expression_idents(&poll.until, &mut refs.expression_idents);
            for s in [&poll.interval_ms, &poll.timeout_ms, &poll.max_error_retries]
                .into_iter()
                .flatten()
            {
                scan_param_refs(s, &mut refs);
            }
        }
        // SRD-13f Push D parallel: phase-level `bindings:` also
        // sit on their own scope post-Push-D; scan for param
        // refs so a phase-binding-only consumer doesn't falsely
        // trip the unused-param check.
        match &phase.bindings {
            nmbrs_workload::model::BindingsDef::PolydatSource(s) => {
                scan_param_refs(s, &mut refs);
                scan_expression_idents(s, &mut refs.expression_idents);
            }
            nmbrs_workload::model::BindingsDef::Map(m) => {
                for v in m.values() {
                    scan_param_refs(v, &mut refs);
                }
            }
        }
        // SRD-83 / SRD-101 — breaker predicates can consume a workload param,
        // but by DIFFERENT mechanisms, so scan each for the shape it actually
        // supports (a param used in the UNsupported shape stays flagged):
        //   - `stop_when` substitutes `{param}` before its predicate compiles;
        //     its bound scope can't resolve a bare param wire → `{param}` only.
        //   - `continue_if` resolves BARE wires through its for_iteration
        //     scope-walk (inherited consts/params) → bare idents only.
        for c in &phase.stop_when {
            scan_param_refs(&c.when, &mut refs);
        }
        if let Some(ci) = &phase.continue_if {
            scan_expression_idents(&ci.when, &mut refs.expression_idents);
        }
        for op in &phase.ops {
            scan_op(op, &mut refs);
        }
    }

    // Scan scenario tree — every node kind contributes its
    // `{...}`-bearing fields. DoWhile/DoUntil contribute their
    // condition text; ForEach/ForCombinations/ForEachUnion
    // contribute their iteration specs.
    //
    // Scenario-tree `{name}` placeholders are runtime-interpolated
    // against the outer iter-var scope (the comprehension's
    // enclosing for-each, plus workload params). An unresolved
    // placeholder there surfaces a `path:line:col:` runtime error
    // through the interpolation pipeline — the early
    // workload-level "undeclared placeholder" check would steal
    // that diagnostic with a less specific error. So we route
    // these refs through a side-channel that contributes to the
    // declared-but-unreferenced check (workload params used only
    // in for_each text are still counted as referenced) but NOT
    // to `refs.placeholders` (which drives the strict
    // "must-resolve-now" guard).
    fn scan_scenario_nodes(nodes: &[nmbrs_workload::model::ScenarioNode], refs: &mut ParamRefs) {
        for node in nodes {
            match node {
                nmbrs_workload::model::ScenarioNode::Phase(_) => {}
                nmbrs_workload::model::ScenarioNode::Comprehension {
                    comprehension,
                    children,
                    ..
                } => {
                    // Grammar-based source-reference extraction.
                    // A comprehension clause `eh in eh_values`
                    // carries `eh_values` as a *bare* source
                    // reference (a `Generator`/`WorkloadParamList`
                    // spec), and `(v) in (concat(foo))` carries
                    // `foo` inside a function call. Byte-scanning
                    // for `{name}` misses both. `referenced_source_names`
                    // parses each spec with the Polydat expression
                    // grammar (via `polydat::dsl::refs`) and returns
                    // the free names structurally. These resolve at
                    // runtime, so they count as references (for the
                    // declared-but-unreferenced check) but bypass
                    // the strict undeclared-placeholder guard.
                    //
                    // A dynamic param-list reference like
                    // `limit in {k_{k}_limits}` surfaces as the
                    // composite name `k_{k}_limits` (the inner
                    // `{k}` is an iter-var hole filled at runtime).
                    // Route composite names — those still carrying
                    // a `{` — into `templates` so the structured
                    // `template_matches` name-composition grammar
                    // resolves them against `k_10_limits` /
                    // `k_100_limits` / …; plain names go to the
                    // runtime-placeholder set.
                    for name in comprehension.referenced_source_names() {
                        if name.contains('{') {
                            refs.templates.push(name);
                        } else {
                            refs.runtime_only_placeholders.insert(name);
                        }
                    }
                    scan_scenario_nodes(children, refs);
                }
                nmbrs_workload::model::ScenarioNode::DoWhile {
                    condition,
                    children,
                    ..
                }
                | nmbrs_workload::model::ScenarioNode::DoUntil {
                    condition,
                    children,
                    ..
                } => {
                    scan_param_refs(condition, refs);
                    scan_scenario_nodes(children, refs);
                }
                nmbrs_workload::model::ScenarioNode::IncludedScenario { children, .. } => {
                    scan_scenario_nodes(children, refs);
                }
                nmbrs_workload::model::ScenarioNode::Bindings { source, children } => {
                    // Scenario-tree `bindings:` (and the `set:`
                    // sugar form) carries Polydat matter text. Scan
                    // the body for `{name}` placeholders and
                    // bare identifiers and route through
                    // `runtime_only_placeholders` so a param
                    // referenced only by a bindings body still
                    // counts as referenced, without tripping the
                    // strict undeclared-placeholder guard (the
                    // body is resolved at kernel build time,
                    // not at op-template substitution).
                    let mut deferred = ParamRefs::default();
                    scan_param_refs(source, &mut deferred);
                    refs.runtime_only_placeholders.extend(deferred.placeholders);
                    refs.expression_idents.extend(deferred.expression_idents);
                    refs.templates.extend(deferred.templates);
                    scan_scenario_nodes(children, refs);
                }
            }
        }
    }
    for nodes in workload.scenarios.values() {
        scan_scenario_nodes(nodes, &mut refs);
    }

    refs
}

/// Collect every iter-var name introduced by a `for_each:` /
/// `for_combinations:` clause anywhere in the scenario tree.
///
/// These names become legitimate `{name}` placeholders inside
/// phases reached via that for-clause — the runner binds them
/// fresh per iteration through the workload kernel's
/// scope-coordinate mechanism. The "referenced but undeclared"
/// validator consults this set so it doesn't false-positive on
/// `{k}` / `{limit}` / `{profile}` references that are clearly
/// satisfied by an enclosing `for_each: "k in …, limit in …,
/// profile in …"`.
///
/// Walks every scenario in the workload (the union — any
/// scenario the operator might invoke), so the validator
/// remains correct regardless of which `scenario=` argument
/// the operator passes on the CLI.
fn collect_iter_var_names(
    workload: &nmbrs_workload::model::Workload,
) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for nodes in workload.scenarios.values() {
        for node in nodes {
            collect_iter_vars_recursive(node, &mut out);
        }
    }
    // Phase-level `for_each:` declarations also introduce iter-vars
    // — `phases.X.for_each: "k in 1, 2, 3"` lets the phase's ops
    // reference `{k}`. The scenario walker doesn't traverse phase
    // bodies, so harvest from each phase's `for_each` clause too.
    for phase in workload.phases.values() {
        if let Some(text) = phase.for_each.as_deref()
            && let Ok(comp) =
                polydat::iteration::comprehension::spec::parse_comprehension_text(text)
        {
            for name in comp.coordinate_names() {
                out.insert(name.to_string());
            }
        }
    }
    out
}

fn collect_iter_vars_recursive(
    node: &nmbrs_workload::model::ScenarioNode,
    out: &mut std::collections::HashSet<String>,
) {
    use nmbrs_workload::model::ScenarioNode::*;
    match node {
        Phase(_) => {}
        Comprehension {
            comprehension,
            children,
            ..
        } => {
            for name in comprehension.coordinate_names() {
                out.insert(name.to_string());
            }
            for child in children {
                collect_iter_vars_recursive(child, out);
            }
        }
        DoWhile {
            children, counter, ..
        }
        | DoUntil {
            children, counter, ..
        } => {
            // `counter:` introduces a bare iteration index name
            // that's legitimately referenceable inside the loop
            // body, even though there's no `for_each` clause.
            if let Some(c) = counter {
                out.insert(c.clone());
            }
            for child in children {
                collect_iter_vars_recursive(child, out);
            }
        }
        IncludedScenario { children, .. } => {
            for child in children {
                collect_iter_vars_recursive(child, out);
            }
        }
        // Scenario-tree `bindings:` (and `set:` sugar) doesn't
        // introduce an iter-var; it publishes a scope-local
        // binding layer. Just walk children.
        Bindings { children, .. } => {
            for child in children {
                collect_iter_vars_recursive(child, out);
            }
        }
    }
}

/// Collect every binding LHS name (wire output) declared in GK
/// source anywhere in the workload — top-level `bindings:`,
/// per-phase `bindings:`, and per-op `bindings:`.
///
/// These names become legitimate `{name}` placeholders inside
/// op text (op-template `prepared:` / `raw:` strings get
/// `{wire}` interpolated to the wire's value at cycle time, just
/// like workload params get expanded earlier in the pipeline).
/// The "referenced but undeclared" validator consults this set so
/// it doesn't false-positive on `{query_vector}` / `{dim}` /
/// `{ground_truth}` references that are clearly satisfied by an
/// enclosing `bindings:` block.
///
/// Scanner is line-based and recognises six shapes:
///   * `input NAME[: TYPE]` — kernel input slot (bare form)
///   * `input (NAME[: TYPE], ...)` — kernel input slot (tuple form)
///   * `const NAME := …`     — init binding (eager, once per scope)
///   * `cursor NAME = …`   — cursor declaration
///   * `shared NAME := …`  — shared output (cross-scope cell)
///   * `const NAME := …`   — final binding
///   * `NAME := …`         — ordinary `:=` output binding
///
/// The collector also mirrors the workload-root kernel's auto-input
/// behaviour (see `bindings.rs::compile_workload_kernel`): when no
/// Polydat source anywhere in the workload declares any `input` slot,
/// the runtime injects `input cycle: u64` so `{cycle}` resolves at
/// op-template substitution time. Reflecting that injection in the
/// validator allow-set prevents false-positive rejection of
/// `{cycle}` placeholders in workloads with no explicit `bindings:`
/// block (e.g. inline `op="tick={cycle}"`).
fn collect_polydat_binding_names(
    workload: &nmbrs_workload::model::Workload,
) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    use nmbrs_workload::model::BindingsDef;

    let mut any_input_decl = false;
    let mut scan_bindings =
        |bindings: &BindingsDef, sink: &mut std::collections::HashSet<String>| {
            match bindings {
                BindingsDef::PolydatSource(s) => {
                    scan_polydat_binding_lhs(sink, s);
                    if s.lines().any(|l| l.trim_start().starts_with("input ")) {
                        any_input_decl = true;
                    }
                }
                BindingsDef::Map(m) => {
                    // Legacy nosqlbench-style chains (`Hash(); Mod(...)`)
                    // are translated into Polydat bindings at runtime by
                    // `compile_bindings_with_opts`; the keys of the map
                    // become the wire names those translations produce.
                    // The validator's allow-set must reflect those names
                    // so referencing `{user_id}` in op text — where
                    // `user_id: Hash(); Mod(...)` is the binding key —
                    // doesn't trip the undeclared-placeholder guard.
                    for name in m.keys() {
                        sink.insert(name.clone());
                    }
                }
            }
        };

    scan_bindings(&workload.bindings, &mut out);
    // Top-level `workload.ops` carry their own `bindings:` blocks
    // in inline-mode workloads (the inline parser puts
    // synthesised `__inline_N := <expr>` lines there). Without
    // walking this collection, the validator misses every
    // inline-rewrite-generated wire name.
    for op in &workload.ops {
        scan_bindings(&op.bindings, &mut out);
    }
    for phase in workload.phases.values() {
        scan_bindings(&phase.bindings, &mut out);
        for op in &phase.ops {
            scan_bindings(&op.bindings, &mut out);
        }
    }

    // Runtime mirror: workload-root kernel auto-injects
    // `input cycle: u64` when no `input` line is declared anywhere.
    // Surface that injection to the validator.
    if !any_input_decl {
        out.insert("cycle".to_string());
    }
    out
}

/// One occurrence of an invalid `{name}` placeholder inside a
/// Polydat expression context. The `location` describes which source
/// block in the workload (e.g. `"phase 'ann_query' bindings"`),
/// and the `placeholder` is the literal body that appeared inside
/// the offending `{...}` — kept verbatim so the error formatter
/// can locate the exact YAML line by substring search.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PolydatBraceFinding {
    location: String,
    placeholder: String,
}

/// Collect every `{name}` placeholder that appears inside a GK
/// source context but OUTSIDE a string literal — these will
/// always fail Polydat compile because `{...}` isn't valid expression
/// syntax. Each finding names the workload block it came from
/// so the error formatter can point at the offending site.
///
/// Walks every Polydat source in the workload: top-level `bindings:`,
/// per-phase `bindings:`, per-op `bindings:`.
fn collect_polydat_brace_refs(
    workload: &nmbrs_workload::model::Workload,
) -> Vec<PolydatBraceFinding> {
    use nmbrs_workload::model::BindingsDef;
    let mut out: Vec<PolydatBraceFinding> = Vec::new();
    // Braces are no longer categorically invalid in Polydat: the block form of
    // conditional selection (`if <cond> { a } else { b }`) uses them. So a brace is
    // only evidence of a stray YAML placeholder when the source ALSO fails to parse.
    // Gating on parseability keeps this check doing its actual job — converting a
    // cryptic "expected expression, got LBrace" into a message that names the file,
    // line and placeholder — without rejecting valid if-expressions.
    let mut push_refs = |loc: &str, source: &str| {
        let parses = polydat::dsl::lexer::lex(source)
            .ok()
            .and_then(|toks| polydat::dsl::parser::parse(toks).ok())
            .is_some();
        if parses {
            return;
        }
        for name in scan_polydat_braced_refs(source) {
            out.push(PolydatBraceFinding {
                location: loc.to_string(),
                placeholder: name,
            });
        }
    };
    if let BindingsDef::PolydatSource(s) = &workload.bindings {
        push_refs("workload `bindings:`", s);
    }
    for (phase_name, phase) in &workload.phases {
        if let BindingsDef::PolydatSource(s) = &phase.bindings {
            push_refs(&format!("phase '{phase_name}' bindings"), s);
        }
        for op in &phase.ops {
            if let BindingsDef::PolydatSource(s) = &op.bindings {
                push_refs(&format!("phase '{phase_name}' op-bindings"), s);
            }
        }
    }
    out
}

/// Scan the raw YAML source for the first line that contains
/// the given placeholder text (with surrounding braces).
/// Returns the 1-based line number when found, `None` otherwise.
///
/// Used to upgrade the GK-brace validator's error message from
/// "phase 'foo' bindings" to a file:line locator the operator
/// can click on or jump to. Works because YAML block scalars
/// (`|` / `>`) preserve the body verbatim; the offending
/// `{name}` substring appears in the file exactly as the
/// validator captured it.
///
/// Falls back to `None` on the rare cases where the literal
/// also appears in an unrelated comment or string — the
/// validator's caller treats that as "give the location but
/// no line number." The substring is namespaced enough
/// (`{name}` with curly braces) that collisions are unlikely
/// in practice.
fn find_yaml_line_for_brace(yaml_source: &str, placeholder: &str) -> Option<usize> {
    let needle = format!("{{{placeholder}}}");
    yaml_source
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(&needle))
        .map(|(idx, _)| idx + 1)
}

/// Scan Polydat source for `{name}` placeholders that appear OUTSIDE
/// string literals and OUTSIDE comments. Inside `"..."` or
/// `'...'` (with backslash escapes), `{...}` is part of the
/// string and gets handled by runtime interpolation — leave it
/// alone. Lines starting with `#` (and content after `#` on any
/// line) are comments — also skipped.
fn scan_polydat_braced_refs(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // Line comment — skip to next newline.
        if b == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // String literal — skip past the closing quote.
        if b == b'"' || b == b'\'' {
            let quote = b;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        // Outside any string — `{` opens a brace placeholder.
        if b == b'{' {
            // Find the matching `}`, allowing one level of
            // nesting for composite forms like `{k_{k}_limits}`.
            // We only need the OUTER body for the error message
            // — the inner placeholders are also wrong but the
            // outer is what the operator sees.
            let start = i + 1;
            let mut depth = 1;
            let mut j = start;
            while j < bytes.len() && depth > 0 {
                match bytes[j] {
                    b'{' => depth += 1,
                    b'}' => depth -= 1,
                    _ => {}
                }
                if depth == 0 {
                    break;
                }
                j += 1;
            }
            if depth == 0 && j > start {
                let body = &source[start..j];
                // Trim whitespace; ignore obviously-empty bodies.
                let trimmed = body.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
                i = j + 1;
                continue;
            }
            // Unmatched `{` — let the Polydat parser handle it with
            // its own error; don't double-report.
            break;
        }
        i += 1;
    }
    out
}

/// Line-by-line scan of Polydat source for locally-bound names —
/// `cursor NAME = …`, `shared NAME := …`, `const NAME := …`,
/// `volatile NAME := …`, bare `NAME := …` assignments, and
/// `input NAME[: TYPE]` / `input (NAME[: TYPE], ...)` declarations.
/// Skips comments and blank lines. Lines that don't match any of
/// these shapes (function-call statements, comments, expression
/// continuations) are ignored.
fn scan_polydat_binding_lhs(out: &mut std::collections::HashSet<String>, source: &str) {
    for raw_line in source.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `input` declarations: collect declared slot names without
        // requiring an `=` / `:=` suffix. Both bare (`input cycle: u64`)
        // and tuple (`input (cycle: u64, q: f64)`) forms are handled.
        if let Some(rest) = line.strip_prefix("input ") {
            scan_input_decl_names(out, rest.trim());
            continue;
        }
        // `extern` declarations (`extern name: type [= default]`,
        // `extern (a: u64, b: f64)`) declare wire names just like
        // `input` — same `name: type` shape. Without this, a
        // `{name}` placeholder referencing an extern-declared wire
        // (e.g. a same-op capture target) trips the
        // undeclared-placeholder guard.
        if let Some(rest) = line.strip_prefix("extern ") {
            scan_input_decl_names(out, rest.trim());
            continue;
        }
        // Strip leading modifier (`const `, `cursor `, `shared `,
        // `volatile `); body is what follows. Modifiers can be
        // combined in a few cases (e.g. `shared const`,
        // `shared volatile`), so loop until no recognised prefix
        // remains.
        let mut body = line;
        loop {
            let stripped = body
                .strip_prefix("const ")
                .or_else(|| body.strip_prefix("cursor "))
                .or_else(|| body.strip_prefix("shared "))
                .or_else(|| body.strip_prefix("volatile "));
            match stripped {
                Some(rest) => body = rest,
                None => break,
            }
        }
        // Tuple-destructure LHS: `(a, b, c) := <expr>`. Each
        // identifier inside the parens becomes a separate
        // declared name. Used by multi-output stdlib nodes
        // (e.g. `(y, mo, d, h, mi, s, ms) := date_components(0)`)
        // and any other binding that unpacks multiple outputs.
        if body.starts_with('(') {
            if let Some(close) = body.find(')') {
                let after = body[close + 1..].trim_start();
                if after.starts_with(":=") || after.starts_with('=') {
                    for raw in body[1..close].split(',') {
                        let name = raw.trim();
                        if !name.is_empty() {
                            out.insert(name.to_string());
                        }
                    }
                }
            }
            continue;
        }
        // Pull the leading identifier.
        let bytes = body.as_bytes();
        let mut i = 0;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        if i == 0 {
            continue;
        }
        // What follows the identifier? Skip whitespace.
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        // Must be `=` (init/cursor) or `:=` (assignment); reject
        // anything else (function calls, expressions starting
        // with an ident, etc.).
        let is_binding = bytes.get(j) == Some(&b'=')
            || (bytes.get(j) == Some(&b':') && bytes.get(j + 1) == Some(&b'='));
        if is_binding {
            out.insert(body[..i].to_string());
            continue;
        }
        // Typed cell form: `name: type := default` (e.g.
        // `shared sstables: u64 := 0`). The `:` here is a type
        // annotation, not `:=` — skip the type token and accept
        // iff an assignment follows it. Without this arm, typed
        // shared/volatile declarations were invisible to the
        // undeclared-placeholder validator and any `{name}`
        // reference to one tripped a false positive.
        if bytes.get(j) == Some(&b':') {
            let rest = body[j + 1..].trim_start();
            let te = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            if te > 0 {
                let after_ty = rest[te..].trim_start();
                if after_ty.starts_with(":=") || after_ty.starts_with('=') {
                    out.insert(body[..i].to_string());
                }
            }
        }
    }
}

/// Parse the name(s) out of an `input` declaration body (the text
/// after the `input ` keyword has been stripped).
///
/// Accepts both surface forms:
/// - `cycle` / `cycle: u64`        → inserts `cycle`
/// - `(a: u64, b: f64, ...)`        → inserts each declared name
///
/// Mirrors `parse_input_decl` in the Polydat parser; this is a
/// lightweight scanner used by scope-elision to register
/// locally-bound names without re-running the full lexer/parser.
fn scan_input_decl_names(out: &mut std::collections::HashSet<String>, body: &str) {
    let body = body.trim();
    if let Some(inner) = body.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        for part in inner.split(',') {
            let name = part.trim().split(':').next().unwrap_or("").trim();
            if !name.is_empty() {
                out.insert(name.to_string());
            }
        }
        return;
    }
    let name = body.split(':').next().unwrap_or("").trim();
    if !name.is_empty() {
        out.insert(name.to_string());
    }
}

/// Resolve a config value to u64 via Polydat scope lookup or numeric parsing.
pub fn resolve_polydat_config(value: &str, kernel: &polydat::kernel::PolydatKernel) -> Option<u64> {
    if value.starts_with('{') && value.ends_with('}') {
        let inner = &value[1..value.len() - 1];
        // SRD-16 §"Visibility Rules: Shadowing": `lookup`
        // walks own folded outputs first then the cell-aware
        // input slot, so a config reference like `{cycles}`
        // resolves whether `cycles` is a folded constant or
        // an extern bound from an outer scope. The previous
        // `get_constant` shape only saw the folded tier, so
        // configs referencing iter-vars or workload params
        // silently fell through to `eval_const_expr`.
        if let Some(v) = kernel.lookup(inner) {
            return Some(value_to_u64(&v));
        }
        match polydat::dsl::compile::eval_const_expr(inner) {
            Ok(v) => Some(value_to_u64(&v)),
            Err(e) => {
                crate::diag!(
                    crate::observer::LogLevel::Error,
                    "error: const expression failed: '{{{inner}}}'"
                );
                crate::diag!(crate::observer::LogLevel::Error, "  {e}");
                None
            }
        }
    } else {
        parse_count(value)
    }
}

/// Convert a Polydat Value to u64, handling f64→u64 truncation.
fn value_to_u64(v: &polydat::ast::Value) -> u64 {
    match v {
        polydat::ast::Value::U64(n) => *n,
        polydat::ast::Value::F64(f) => *f as u64,
        polydat::ast::Value::Bool(b) => {
            if *b {
                1
            } else {
                0
            }
        }
        _ => 0,
    }
}

/// Resolve a scenario name to a list of phase names.
fn resolve_scenario(
    scenarios: &HashMap<String, Vec<nmbrs_workload::model::ScenarioNode>>,
    phase_order: &[String],
    name: &str,
) -> Result<Vec<nmbrs_workload::model::ScenarioNode>, String> {
    if let Some(nodes) = scenarios.get(name) {
        return Ok(nodes.clone());
    }
    if name == "default" && !phase_order.is_empty() {
        return Ok(phase_order
            .iter()
            .map(|n| nmbrs_workload::model::ScenarioNode::Phase(n.clone()))
            .collect());
    }
    Err(format!("scenario '{name}' not found"))
}

/// Format a scenario tree for display — one construct per line,
/// nested with two-space indent per level. Phases include their
/// declared `cycles:` and `concurrency:` config when available
/// from the workload's phase map so the operator can see the
/// run shape at a glance.
///
/// Example output for the full_cql_vector fulltest scenario:
///
/// ```text
/// scenario 'test_oracles'
///   for_each profile in matching_profiles('{dataset}', '{oracles_prefix}')
///     for_each table in vec_{profile}
///       teardown                          (cycles: 1, concurrency: 1)
///       schema                            (cycles: 1, concurrency: 1)
///       rampup
///       jolokia_flush
///       for_combinations [k, limit] in {k_values}, {k_{k}_limits}
///         ann_query                       (concurrency: {query_concurrency})
/// scenario 'test_fknn'
///   ...
/// recall_audit_oracle
/// recall_audit_pvs
/// ```
///
/// The replaced LISP-shaped one-liner had bracket nesting that
/// scaled badly past two levels and required mental parsing to
/// see the loop structure.
fn format_scenario_tree(
    nodes: &[nmbrs_workload::model::ScenarioNode],
    phases: &std::collections::HashMap<String, nmbrs_workload::model::WorkloadPhase>,
) -> String {
    let mut out = String::new();
    format_scenario_nodes(nodes, phases, 0, &mut out);
    // Trim trailing newline so the runner's log call doesn't
    // emit a double-blank line.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Render a multi-coord comprehension's `[vars] in [specs]`
/// in two column-aligned lines. Column `i` is padded to the
/// widest of `vars[i]` and `specs[i]` so corresponding entries
/// stack vertically:
///
/// ```text
/// for [sm,          mnc,          bw,          eh,           alf_label]
///  in [{sm_values}, {mnc_values}, {bw_values}, {eh_values}, concat({alf_label_values})]
/// ```
///
/// `for ` and ` in ` are 4 chars (padding `in` with a leading
/// space) so the `[` brackets and every column thereafter
/// share the same vertical line. Color highlights the
/// keywords when the active terminal supports it; on a
/// piped/no-color stderr the output stays plain.
///
/// `indent_prefix` is the per-depth indent at the call site
/// — applied to the second line so it sits at the same depth
/// as the first.
fn format_for_combinations(pairs: &[(String, String)], indent_prefix: &str, color: bool) -> String {
    let kw_open = if color { "\x1b[1;36m" } else { "" };
    let kw_close = if color { "\x1b[0m" } else { "" };
    let bracket_open = if color { "\x1b[2m" } else { "" };
    let bracket_close = if color { "\x1b[0m" } else { "" };

    let widths: Vec<usize> = pairs
        .iter()
        .map(|(v, s)| v.chars().count().max(s.chars().count()))
        .collect();

    let pad = |entry: &str, idx: usize, last: bool| -> String {
        // Last column gets no trailing comma + no padding —
        // the closing `]` lands flush against the final token.
        if last {
            entry.to_string()
        } else {
            // `<entry>,` then pad to `widths[idx] + 1` so the
            // next column begins at a constant offset.
            let with_comma = format!("{entry},");
            let width_target = widths[idx] + 1; // +1 for the comma
            let visible = with_comma.chars().count();
            if visible >= width_target {
                with_comma
            } else {
                format!("{with_comma}{:<pad$}", "", pad = width_target - visible)
            }
        }
    };

    let last_idx = pairs.len().saturating_sub(1);
    let vars_line: String = pairs
        .iter()
        .enumerate()
        .map(|(i, (v, _))| pad(v, i, i == last_idx))
        .collect::<Vec<_>>()
        .join(" ");
    let specs_line: String = pairs
        .iter()
        .enumerate()
        .map(|(i, (_, s))| pad(s, i, i == last_idx))
        .collect::<Vec<_>>()
        .join(" ");

    format!(
        "{kw_open}for{kw_close} {bracket_open}[{bracket_close}{vars_line}{bracket_open}]{bracket_close}\n\
         {indent_prefix} {kw_open}in{kw_close} {bracket_open}[{bracket_close}{specs_line}{bracket_open}]{bracket_close}"
    )
}

fn format_scenario_nodes(
    nodes: &[nmbrs_workload::model::ScenarioNode],
    phases: &std::collections::HashMap<String, nmbrs_workload::model::WorkloadPhase>,
    depth: usize,
    out: &mut String,
) {
    use nmbrs_workload::model::ScenarioNode::*;
    let indent = " ".repeat(depth);
    for node in nodes {
        match node {
            Phase(name) => {
                let suffix = phases
                    .get(name)
                    .map(format_phase_config_suffix)
                    .unwrap_or_default();
                if suffix.is_empty() {
                    out.push_str(&format!("{indent}{name}\n"));
                } else {
                    // Pad name to a fixed column so the
                    // `(cycles:..., concurrency:...)` chip lines
                    // up across consecutive phases. 32 chars
                    // covers the typical phase names; longer
                    // names just push past the column without
                    // breaking layout.
                    out.push_str(&format!("{indent}{name:<32} {suffix}\n",));
                }
            }
            Comprehension {
                comprehension,
                children,
                ..
            } => {
                // Algebra-native display: walk the AST once to
                // detect Union vs flat, then format. Matches
                // the scope_tree::label_for_comprehension shape
                // at one level of detail finer (includes the
                // spec_expr for non-Union shapes).
                use polydat::iteration::comprehension::Comprehension as Comp;
                // Peel outer Order/Filter for structural detection.
                let mut body = comprehension;
                while let Comp::Order { child, .. } | Comp::Filter { child, .. } = body {
                    body = child;
                }
                let header = match body {
                    Comp::Union { children } => {
                        let names = comprehension.coordinate_names().join(", ");
                        format!("for_each_union [{}] ({} sub-spaces)", names, children.len())
                    }
                    _ => {
                        let pairs = comprehension.coordinate_specs();
                        if pairs.len() == 1 {
                            let (var, spec) = &pairs[0];
                            format!("for_each {var} in {spec}")
                        } else {
                            // Two-line column-aligned form for
                            // multi-coord comprehensions: variable
                            // names on the first line, source
                            // expressions on the second, each
                            // column padded to its widest
                            // (var, spec) pair so the columns
                            // line up vertically. Keywords
                            // `for` / ` in` are right-aligned
                            // so the `[` brackets land in the
                            // same column.
                            format_for_combinations(&pairs, &indent, crate::observer::use_color())
                        }
                    }
                };
                out.push_str(&format!("{indent}{header}\n"));
                format_scenario_nodes(children, phases, depth + 1, out);
            }
            DoWhile {
                condition,
                counter,
                children,
            } => {
                let ctr = counter
                    .as_deref()
                    .map(|c| format!(" (counter={c})"))
                    .unwrap_or_default();
                out.push_str(&format!("{indent}do_while '{condition}'{ctr}\n"));
                format_scenario_nodes(children, phases, depth + 1, out);
            }
            DoUntil {
                condition,
                counter,
                children,
            } => {
                let ctr = counter
                    .as_deref()
                    .map(|c| format!(" (counter={c})"))
                    .unwrap_or_default();
                out.push_str(&format!("{indent}do_until '{condition}'{ctr}\n"));
                format_scenario_nodes(children, phases, depth + 1, out);
            }
            IncludedScenario { name, children } => {
                out.push_str(&format!("{indent}scenario '{name}'\n"));
                format_scenario_nodes(children, phases, depth + 1, out);
            }
            Bindings { source, children } => {
                // First non-empty line of the source as a one-
                // line summary in the scenario-tree dump. Long
                // bodies stay readable in the YAML; the
                // hierarchical view just teases the binding.
                let summary = source
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("");
                if source.lines().filter(|l| !l.trim().is_empty()).count() > 1 {
                    out.push_str(&format!("{indent}bindings: {summary} …\n"));
                } else {
                    out.push_str(&format!("{indent}bindings: {summary}\n"));
                }
                format_scenario_nodes(children, phases, depth + 1, out);
            }
        }
    }
}

/// Render the `(cycles: X, concurrency: Y)` suffix for a phase
/// line in the scenario-tree summary. Includes only the fields
/// that the phase actually declared — phases that inherit the
/// runtime defaults skip the chip entirely so the tree line
/// stays uncluttered. Strings are shown verbatim (including
/// `{name}` placeholders) so the operator sees the workload's
/// declared intent rather than a runtime-evaluated number.
fn format_phase_config_suffix(phase: &nmbrs_workload::model::WorkloadPhase) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(c) = phase.cycles.as_deref()
        && !c.is_empty()
    {
        parts.push(format!("cycles: {c}"));
    }
    if let Some(c) = phase.concurrency.as_deref()
        && !c.is_empty()
    {
        parts.push(format!("concurrency: {c}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("({})", parts.join(", "))
    }
}

/// SRD-108 Part B — load a secondary workload document (an
/// `implements:` target, or the `impl=` module) through the same
/// resolution the primary `workload=` uses: local file first,
/// then the bundled catalog. Returns the parsed workload plus its
/// canonical identity string (canonicalized path, or catalog
/// name) for target-matching.
fn load_secondary_workload(
    reference: &str,
    params: &HashMap<String, String>,
    base_dir: Option<&std::path::Path>,
    bundled_origin: Option<&str>,
) -> Result<(nmbrs_workload::model::Workload, String), String> {
    match resolve_secondary_ref(reference, base_dir, bundled_origin)? {
        ResolvedWorkload::Path(path) => {
            let workload = nmbrs_workload::parse::parse_workload_from_path(
                std::path::Path::new(&path),
                params,
            )
            .map_err(|e| format!("parse workload '{path}': {e}"))?;
            Ok((workload, canonical_identity(&path)))
        }
        ResolvedWorkload::Bundled(bundled) => {
            let (merged, res_warnings) =
                nmbrs_workload::extends::load_and_merge_bundled(bundled)
                    .map_err(|e| format!("bundled workload `{}`: {e}", bundled.name))?;
            let mut workload = nmbrs_workload::parse::parse_workload(&merged, params)
                .map_err(|e| format!("parse bundled workload `{}`: {e}", bundled.name))?;
            workload.resolution_warnings.extend(res_warnings);
            Ok((workload, bundled.name.to_string()))
        }
    }
}

/// Resolve a workload reference to its canonical identity WITHOUT
/// parsing it — used to compare an `implements:` target against
/// the invoked `workload=`.
fn workload_ref_identity(
    reference: &str,
    base_dir: Option<&std::path::Path>,
    bundled_origin: Option<&str>,
) -> Result<String, String> {
    match resolve_secondary_ref(reference, base_dir, bundled_origin)? {
        ResolvedWorkload::Path(path) => Ok(canonical_identity(&path)),
        ResolvedWorkload::Bundled(bundled) => Ok(bundled.name.to_string()),
    }
}

/// SRD-109 Part 2 — resolve a driver manifest by name: local
/// `drivers/<name>/driver.yaml` under the cwd first, then the
/// bundled catalog entry `drivers/<name>/driver`. Both at once
/// FAVORS the local manifest (SRD-85 nearest-first) with a
/// logged warning naming both — shadowing is allowed but never
/// silent. Returns the manifest plus the resolved LIBRARY
/// reference: a file path for a local manifest, a catalog name
/// for a bundled one. `None` when neither exists — the caller
/// falls back to the legacy driver-as-adapter-alias meaning.
fn resolve_driver_manifest(
    name: &str,
) -> Result<Option<(nmbrs_workload::drivers::DriverManifest, String)>, String> {
    let local_path = std::path::Path::new("drivers")
        .join(name)
        .join("driver.yaml");
    let catalog_name = format!("drivers/{name}/driver");
    let bundled = nmbrs_workload::catalog::lookup(&catalog_name);
    match (local_path.is_file(), bundled) {
        (true, Some(_)) => {
            crate::observer::log_tagged(
                crate::observer::LogLevel::Warn,
                crate::observer::EventTag::in_flight(crate::observer::EventCategory::Resolution),
                &format!(
                    "resolve: driver '{name}' matches multiple resources — local \
                 manifest {} AND bundled driver `{catalog_name}` — using the \
                 local manifest (filesystem-first). Same-named resources in \
                 multiple places invite confusion: prefer a unique name.",
                    local_path.display()
                ),
            );
            let source = std::fs::read_to_string(&local_path)
                .map_err(|e| format!("read driver manifest {}: {e}", local_path.display()))?;
            let manifest = nmbrs_workload::drivers::parse_driver_manifest(
                &source,
                &local_path.display().to_string(),
            )?;
            verify_driver_identity(&manifest, name)?;
            let dir = local_path.parent().expect("manifest path has a parent");
            let library_ref = resolve_driver_library_local(dir, &manifest.library)?;
            Ok(Some((manifest, library_ref)))
        }
        (true, None) => {
            let source = std::fs::read_to_string(&local_path)
                .map_err(|e| format!("read driver manifest {}: {e}", local_path.display()))?;
            let manifest = nmbrs_workload::drivers::parse_driver_manifest(
                &source,
                &local_path.display().to_string(),
            )?;
            verify_driver_identity(&manifest, name)?;
            let dir = local_path.parent().expect("manifest path has a parent");
            let library_ref = resolve_driver_library_local(dir, &manifest.library)?;
            Ok(Some((manifest, library_ref)))
        }
        (false, Some(entry)) => {
            let manifest =
                nmbrs_workload::drivers::parse_driver_manifest(entry.source, &catalog_name)?;
            verify_driver_identity(&manifest, name)?;
            let stem = manifest
                .library
                .strip_suffix(".yaml")
                .or_else(|| manifest.library.strip_suffix(".yml"))
                .unwrap_or(&manifest.library);
            let library_ref = format!("drivers/{name}/{stem}");
            Ok(Some((manifest, library_ref)))
        }
        (false, None) => Ok(None),
    }
}

/// A manifest must be named for the directory it lives in — a
/// mismatch is a packaging bug surfaced at load, not a silent
/// re-route.
fn verify_driver_identity(
    manifest: &nmbrs_workload::drivers::DriverManifest,
    name: &str,
) -> Result<(), String> {
    if manifest.driver != name {
        return Err(format!(
            "driver manifest for '{name}' declares `driver: {}` — the \
             manifest must be named for its directory",
            manifest.driver
        ));
    }
    Ok(())
}

/// Resolve a local manifest's `library:` reference beside the
/// manifest: as written first, then with `.yaml` appended.
fn resolve_driver_library_local(dir: &std::path::Path, library: &str) -> Result<String, String> {
    let as_written = dir.join(library);
    if as_written.is_file() {
        return Ok(as_written.display().to_string());
    }
    let with_ext = dir.join(format!("{library}.yaml"));
    if with_ext.is_file() {
        return Ok(with_ext.display().to_string());
    }
    Err(format!(
        "driver manifest {}: library '{library}' not found beside the manifest",
        dir.display()
    ))
}

/// Apply a resolved driver manifest to the invocation params:
/// the library lands as `workload=` (no workload given) or
/// `impl=` (a blueprint was invoked); the backing adapter and the
/// manifest's default params fill only ABSENT keys, so the CLI
/// always wins and the defaults still overlay the library's own
/// declared params (this map is the CLI overlay layer).
fn apply_driver_manifest(
    params: &mut HashMap<String, String>,
    driver_name: &str,
    manifest: nmbrs_workload::drivers::DriverManifest,
    library_ref: String,
    workload_given: bool,
) -> Result<(), String> {
    if params.contains_key("impl") {
        return Err(format!(
            "driver={driver_name} supplies the implementation library \
             ('{}') — impl= conflicts; pass one or the other",
            manifest.library
        ));
    }
    if workload_given {
        params.insert("impl".into(), library_ref.clone());
    } else {
        params.insert("workload".into(), library_ref.clone());
    }
    params
        .entry("adapter".to_string())
        .or_insert_with(|| manifest.adapter.clone());
    for (k, v) in &manifest.default_params {
        params.entry(k.clone()).or_insert_with(|| v.clone());
    }
    crate::diag!(
        crate::observer::LogLevel::Info,
        "driver: {driver_name} → adapter={}, library={library_ref}",
        manifest.adapter
    );
    Ok(())
}

/// Resolve a secondary workload reference the way `extends:`
/// targets resolve: relative to the REFERRING DOCUMENT's
/// directory first (when one exists on disk), then the standard
/// cwd-local + bundled-catalog path. Without the base-dir leg, an
/// `implements: ./blueprint.yaml` inside a file would only resolve
/// when the invoking cwd happens to be the file's directory.
fn resolve_secondary_ref(
    reference: &str,
    base_dir: Option<&std::path::Path>,
    bundled_origin: Option<&str>,
) -> Result<ResolvedWorkload, String> {
    // SRD-85 nearest-first: every candidate is enumerated and the
    // nearest wins — the logical filesystem location is favored
    // over the embedded catalog BY DEFAULT (a fresh checkout must
    // never be silently shadowed by a stale binary's catalog).
    // Multiple matches are a warnable condition, logged with every
    // candidate named; never a hard error and never silent.
    let pinned = reference.starts_with("./") || reference.starts_with("../");

    // 1. The referring document's own directory (nearest).
    let origin_file: Option<String> = base_dir
        .map(|dir| dir.join(reference))
        .filter(|c| c.is_file())
        .map(|c| c.display().to_string());

    // A `./`-pinned reference that resolves beside its referring
    // FILE is explicit — no enumeration, no warning.
    if pinned && base_dir.is_some() && origin_file.is_some() {
        return Ok(ResolvedWorkload::Path(origin_file.unwrap()));
    }

    // 2. The cwd's logical layout (exact path, extension probing,
    //    cwd `workloads/`).
    let cwd_file: Option<String> = resolve_workload_file(reference).filter(|p| {
        // Dedupe against the origin-dir candidate.
        origin_file.as_deref().map(canonical_identity) != Some(canonical_identity(p))
    });

    // 3. The bundled catalog: exact name, then the sibling idiom —
    //    the referring bundled document's namespace — then the bare
    //    stem (extension and `./` stripped: files reference siblings
    //    by filename, catalog names carry none).
    let stem = reference
        .strip_suffix(".yaml")
        .or_else(|| reference.strip_suffix(".yml"))
        .unwrap_or(reference);
    let stem = stem.strip_prefix("./").unwrap_or(stem);
    let ns_stem = bundled_origin
        .and_then(|o| o.rsplit_once('/'))
        .map(|(ns, _)| format!("{ns}/{stem}"));
    let bundled: Option<&'static nmbrs_workload::catalog::BundledWorkload> =
        nmbrs_workload::catalog::lookup(reference)
            .or_else(|| ns_stem.as_deref().and_then(nmbrs_workload::catalog::lookup))
            .or_else(|| nmbrs_workload::catalog::lookup(stem));

    let mut names: Vec<String> = Vec::new();
    if let Some(p) = &origin_file {
        names.push(format!("file {p} (beside the referring document)"));
    }
    if let Some(p) = &cwd_file {
        names.push(format!("local file {p}"));
    }
    if let Some(b) = bundled {
        names.push(format!("bundled workload `{}`", b.name));
    }

    if names.len() > 1 {
        crate::observer::log_tagged(
            crate::observer::LogLevel::Warn,
            crate::observer::EventTag::in_flight(crate::observer::EventCategory::Resolution),
            &format!(
                "resolve: reference '{reference}' matches multiple resources — {} — \
             using the nearest ({}). Same-named resources in multiple places \
             invite confusion: prefer a unique name, or pin the intent with a \
             `./` path / full catalog name.",
                names.join(" AND "),
                names[0]
            ),
        );
    }

    if let Some(p) = origin_file {
        return Ok(ResolvedWorkload::Path(p));
    }
    if let Some(p) = cwd_file {
        return Ok(ResolvedWorkload::Path(p));
    }
    if let Some(b) = bundled {
        return Ok(ResolvedWorkload::Bundled(b));
    }
    Err(format!(
        "workload not found: '{reference}'. Not a local file, and no bundled \
         workload by that name — `nmbrs describe workloads` lists what this \
         binary carries.{}",
        nmbrs_workload::suggest::did_you_mean(&nmbrs_workload::suggest::suggest_workloads(
            reference
        )),
    ))
}

/// Canonicalize a workload file path for identity comparison;
/// bundled names pass through unchanged (they contain no path
/// separators that resolve).
fn canonical_identity(reference: &str) -> String {
    std::fs::canonicalize(reference)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| reference.to_string())
}

/// SRD-106 Part 3 — light pre-read of the workload's merged YAML
/// for the top-level `stick_session:` flag. Runs BEFORE session
/// construction (the full workload parse happens after the session
/// exists, so it cannot inform session resolution). Resolution
/// mirrors the execution path — local file first, then the bundled
/// catalog — and `extends:` chains merge before the peek, so a
/// suite base declaring `stick_session: true` reaches derived
/// workloads. Any failure answers `None`; the full parse surfaces
/// the real error with proper context.
fn peek_stick_session(params: &HashMap<String, String>, args: &[String]) -> Option<bool> {
    if params.contains_key("op") {
        return None; // inline workloads carry no header
    }
    let workload_raw = params.get("workload").cloned().or_else(|| {
        args.iter()
            .find(|a| a.ends_with(".yaml") || a.ends_with(".yml"))
            .cloned()
    })?;
    // Pre-probe only (stick_session peek): resolution warnings
    // are dropped here — the authoritative load that follows
    // surfaces the identical warnings itself.
    let merged = match resolve_workload(&workload_raw).ok()? {
        ResolvedWorkload::Path(p) => {
            nmbrs_workload::extends::load_and_merge(std::path::Path::new(&p))
                .ok()?
                .0
        }
        ResolvedWorkload::Bundled(b) => nmbrs_workload::extends::load_and_merge_bundled(b).ok()?.0,
    };
    let doc: serde_yaml::Value = serde_yaml::from_str(&merged).ok()?;
    doc.get("stick_session")?.as_bool()
}

/// Resolve a workload file path from a bare name.
/// Tries: as-is, with .yaml/.yml extension, then under workloads/.
/// SRD-85 resolution result: a local file path or a bundled
/// catalog entry.
pub enum ResolvedWorkload {
    Path(String),
    Bundled(&'static nmbrs_workload::catalog::BundledWorkload),
}

/// Resolve a `workload=` value per SRD-85 nearest-first: local
/// files first (exact path, extension probing, cwd `workloads/`),
/// then the bundled catalog by exact name. A name that resolves
/// both ways FAVORS the logical filesystem location and logs a
/// warning naming both — shadowing is allowed but never silent.
/// `./`-prefixed paths pin the local reading without a warning.
pub fn resolve_workload(name: &str) -> Result<ResolvedWorkload, String> {
    let local = resolve_workload_file(name);
    let bundled = nmbrs_workload::catalog::lookup(name);
    match (local, bundled) {
        (Some(local_path), Some(b)) => {
            if !name.starts_with("./") && !name.starts_with("../") {
                crate::observer::log_tagged(
                    crate::observer::LogLevel::Warn,
                    crate::observer::EventTag::in_flight(
                        crate::observer::EventCategory::Resolution,
                    ),
                    &format!(
                        "resolve: workload '{name}' matches multiple resources — \
                     local file {local_path} AND bundled workload `{}` — using \
                     the local file (filesystem-first). Same-named resources in \
                     multiple places invite confusion: prefer a unique name, or \
                     pin the intent with a `./` path.",
                        b.name
                    ),
                );
            }
            Ok(ResolvedWorkload::Path(local_path))
        }
        (Some(local_path), None) => Ok(ResolvedWorkload::Path(local_path)),
        (None, Some(b)) => Ok(ResolvedWorkload::Bundled(b)),
        (None, None) => Err(format!(
            "workload not found: '{name}'. Not a local file, and no bundled \
             workload by that name — `nmbrs describe workloads` lists what \
             this binary carries.{}",
            nmbrs_workload::suggest::did_you_mean(&nmbrs_workload::suggest::suggest_workloads(
                name
            ),)
        )),
    }
}

fn resolve_workload_file(name: &str) -> Option<String> {
    let p = std::path::Path::new(name);
    if p.exists() {
        return Some(name.to_string());
    }

    // Already has yaml extension — no further search
    if name.ends_with(".yaml") || name.ends_with(".yml") {
        // Try under workloads/
        let under = format!("workloads/{name}");
        if std::path::Path::new(&under).exists() {
            return Some(under);
        }
        return None;
    }

    // Try adding extensions
    for ext in [".yaml", ".yml"] {
        let with_ext = format!("{name}{ext}");
        if std::path::Path::new(&with_ext).exists() {
            return Some(with_ext);
        }
    }

    // Try under workloads/
    for ext in ["", ".yaml", ".yml"] {
        let under = format!("workloads/{name}{ext}");
        if std::path::Path::new(&under).exists() {
            return Some(under);
        }
    }

    None
}

/// Normalize args: detect scenario shorthand where a bare word after
/// the workload file becomes `scenario=<name>`.
///
/// The auto-promotion has to skip the **values** of space-form
/// flags (`--session-path X`, `--readout Y`, etc.) — otherwise
/// the path or value gets misread as a scenario name and ends up
/// as `scenario=<path>`, which downstream code then materialises
/// as a literal directory at `<cwd>/scenario=<path>` (the
/// orphaned-dir bug we hit earlier). Use the same list
/// [`parse_params`] uses so the two surfaces agree on which
/// flags consume their next token.
pub fn normalize_args(args: &[String]) -> Vec<String> {
    // Spec-derived value-taking flags (installed at startup); the
    // session-flag fallback applies for library/test drivers.
    let value_flags = known_value_flags();

    let mut result = Vec::new();
    let mut workload_seen = false;
    let mut scenario_set = false;
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        // Pass through space-form flag + its value as a unit.
        // Equals-form (`--session-path=X`) is one token and
        // skips this branch.
        if value_flags.iter().any(|f| *f == arg) {
            result.push(arg.clone());
            if let Some(next) = iter.next() {
                result.push(next.clone());
            }
            continue;
        }
        if !workload_seen
            && (arg.ends_with(".yaml") || arg.ends_with(".yml") || arg.contains("workload="))
        {
            workload_seen = true;
            result.push(arg.clone());
        } else if workload_seen && !scenario_set && !arg.contains('=') && !arg.starts_with('-') {
            result.push(format!("scenario={arg}"));
            scenario_set = true;
        } else {
            result.push(arg.clone());
        }
    }
    result
}

/// Bare flags accepted by the runner — these don't follow the
/// `key=value` shape but are otherwise recognized. Centralized
/// here so [`parse_params`] doesn't reject them and any consumer
/// can re-check the raw `args` for them.
const RECOGNIZED_BARE_FLAGS: &[&str] = &[
    "--strict",             // SRD-15 strict-mode toggle.
    "--resume-latest",      // SRD-44: resume from logs/latest.
    "--force-retry-failed", // SRD-44: prepend retry,warn to errors.
    "--refine",             // SRD-77: enable refine-mode skip-plan loading.
];

/// Strip a single layer of matching outer quotes (single or
/// double) from a string slice. Idempotent for un-quoted input.
///
/// Per SRD 71 §"CLI parsing — quote elision": this lets wrapper
/// scripts forwarding `"$@"`, or `key="value"` constructions
/// that double-passed through a shell, parse the same as their
/// bare equivalents. Backtick and other quote-like characters
/// are deliberately not handled — they carry shell-evaluation
/// semantics that don't survive into our argv.
pub(crate) fn elide_outer_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return s;
    }
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    if (first == b'\'' || first == b'"') && first == last {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Flags consumed by `crate::session::resolve_session_dir` at startup.
/// They appear in raw `args` but shouldn't reach the per-key params map.
/// Both equals-form (`--session-dir=/path`) and space-form
/// (`--session-dir /path`) are recognised; the space-form value is
/// silently absorbed. Shared by [`parse_params`] and
/// [`detect_conflicting_duplicate_params`] so they treat these args
/// identically.
const SESSION_DIR_FLAGS: &[&str] = &[
    // Umbrella flag (kv-list).
    "--session",
    // Per-key long-form flags.
    "--session-name",
    "--session-path",
    "--session-reuse",
    "--session-keep",
    "--session-shelflife",
    // SRD-63 §8: `--readout=<body>` overrides the workload's
    // `on_update` binding for the run. Resolved by
    // `crate::session::resolve_flag` at runner-init; consumed here so
    // the value doesn't bleed into the workload params map.
    "--readout",
];

/// Reject a `key=value` run param supplied more than once with
/// *conflicting* values. These params collapse into a map (last value
/// wins — see [`parse_params`]), which silently discards an earlier
/// value: e.g. `scenario=reset scenario=idx_sweep` drops `reset` and
/// runs `idx_sweep`. A repeat with an IDENTICAL value is harmless and
/// allowed (re-passing the same value shouldn't break a script); a
/// conflicting repeat is an ambiguous instruction, so it's rejected and
/// surfaced rather than silently last-wins ("Never Ignore Silently").
///
/// Mirrors `parse_params`'s arg walk: session-dir flags (own resolver)
/// and dotted phase-scoped overrides (`<phase>.<param>=`, a separate
/// namespace) are skipped, and the same quote elision is applied so
/// `scenario=x` and `scenario='x'` compare equal.
pub fn detect_conflicting_duplicate_params(args: &[String]) -> Result<(), String> {
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        // Session-dir flags: consumed by the startup hook, absorb the
        // space-form value so it isn't mistaken for a param.
        if known_value_flags()
            .iter()
            .any(|p| arg == p || arg.starts_with(&format!("{p}=")))
        {
            if !arg.contains('=') {
                let _consumed = iter.next();
            }
            continue;
        }
        let unquoted = elide_outer_quotes(arg.as_str());
        let stripped = unquoted.trim_start_matches('-');
        let Some(eq_pos) = stripped.find('=') else {
            continue;
        };
        let key = stripped[..eq_pos].to_string();
        // Dotted (non-path) keys are SRD-71 phase-scoped overrides — a
        // separate namespace — skipped here as in `parse_params`.
        if key.contains('.') && !key.contains('/') && !key.contains('\\') {
            continue;
        }
        let value = elide_outer_quotes(&stripped[eq_pos + 1..]).to_string();
        match seen.get(&key) {
            Some(prev) if *prev != value => {
                return Err(format!(
                    "parameter '{key}' specified more than once with conflicting \
                     values ('{prev}' and '{value}') — pass it exactly once"
                ));
            }
            Some(_) => {} // identical repeat — harmless, allow.
            None => {
                seen.insert(key, value);
            }
        }
    }
    Ok(())
}

/// Parse `key=value` pairs from command line args.
///
/// Quote handling (SRD 71): if the whole arg or just the value
/// portion is wrapped in matching `'…'` / `"…"` quotes, those
/// quotes are stripped. So `cursor=0..53%`, `cursor='0..53%'`,
/// `cursor="0..53%"`, `'cursor=0..53%'`, and `"cursor=0..53%"`
/// all parse to the same `(name="cursor", value="0..53%")`
/// pair. The first `=` still splits name from value, so values
/// containing `=` retain everything after the first split.
pub fn parse_params(args: &[String]) -> HashMap<String, String> {
    let mut params = HashMap::new();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        // Session-dir flags (consumed by the startup hook,
        // not stored in params).
        if known_value_flags()
            .iter()
            .any(|p| arg == p || arg.starts_with(&format!("{p}=")))
        {
            if !arg.contains('=') {
                let _consumed = iter.next();
            }
            continue;
        }

        // Quote elision (SRD 71): strip matching outer quotes
        // from the whole arg first — handles `'key=value'` and
        // `"key=value"` — then again from the value portion
        // after the `=` split — handles `key='value'` and
        // `key="value"`.
        let unquoted = elide_outer_quotes(arg.as_str());
        // Strip leading dashes: --dryrun=phase,wiring → dryrun=phase,wiring
        let stripped = unquoted.trim_start_matches('-');
        if let Some(eq_pos) = stripped.find('=') {
            let key = stripped[..eq_pos].to_string();
            // Dotted keys (without path separators) are SRD-71
            // phase-scoped overrides (`<phase-pattern>.<param>=`),
            // parsed by `crate::phase_params::parse_overrides` —
            // not workload params.
            if key.contains('.') && !key.contains('/') && !key.contains('\\') {
                continue;
            }
            let value = elide_outer_quotes(&stripped[eq_pos + 1..]).to_string();
            params.insert(key, value);
        } else if arg.ends_with(".yaml") || arg.ends_with(".yml") {
            // Workload file path — handled elsewhere
        } else if is_recognized_bare_flag(arg.as_str()) || arg.starts_with("--polydat-lib=") {
            // Bare runner flag — consumed elsewhere via `args`
            // scan (e.g. `--strict`, `--polydat-lib=path`).
        } else {
            crate::diag!(
                crate::observer::LogLevel::Error,
                "error: unrecognized argument '{arg}'. Expected key=value format."
            );
            std::process::exit(1);
        }
    }
    params
}

/// Overlay CLI `key=value` params onto a base set, CLI winning on
/// conflict. The single precedence rule for the whole run — applied to
/// the session-tier effective params ([`effective_params`]) and to the
/// execution's workload params identically, so "CLI overrides the
/// workload" means the same thing everywhere.
fn overlay_cli_params(
    mut base: HashMap<String, String>,
    cli: &HashMap<String, String>,
) -> HashMap<String, String> {
    for (k, v) in cli {
        // Coerce the CLI value to the type already inferred for this key
        // (the declared default, resolved by `parse_workload`), so a
        // suffixed override like `max_size=10m` re-applies as a number
        // rather than overwriting the coerced value with raw text. Keys
        // with no declared default (ad-hoc CLI params) pass through.
        let coerced = match base.get(k) {
            Some(existing) => nmbrs_workload::magnitude::coerce_param_override(existing, v),
            None => v.clone(),
        };
        base.insert(k.clone(), coerced);
    }
    base
}

/// The run's **effective parameters**: the workload's declared top-level
/// `params:` (extends-merged) as the base, with CLI `key=value` args
/// overlaid on top (CLI wins). This is the single consolidated param set —
/// the same one whether a setting is declared in the workload or passed on
/// the command line — used for session-tier services (metrics cadence,
/// push reporters, per-instance metrics) and console-ownership detection
/// alike. The per-execution path reaches the same result by overlaying CLI
/// onto the fully-parsed `workload.params` (see `run_execution`).
///
/// The workload reference is taken from `workload=` or a bare `.yaml`/`.yml`
/// positional; an unresolvable/absent workload contributes no base params
/// (the real load error, if any, surfaces later in the execution).
pub fn effective_params(args: &[String]) -> HashMap<String, String> {
    let cli = parse_params(args);
    let workload_ref = cli.get("workload").cloned().or_else(|| {
        args.iter()
            .find(|a| (a.ends_with(".yaml") || a.ends_with(".yml")) && !a.contains('='))
            .cloned()
    });
    let base = workload_ref
        .as_deref()
        .and_then(nmbrs_workload::verify::declared_params)
        .unwrap_or_default();
    overlay_cli_params(base, &cli)
}

/// Param keys that configure the **session-tier** services — one value per
/// session, shared by every execution under it. Executions that declare
/// different values for any of these cannot share a session; a multi-
/// execution harness must group by them and set up one session per group
/// (see [`session_param_signature`]). `metrics_cadence` is the load-bearing
/// case: it fixes the cadence the optimizer settle detector samples, so
/// workloads wanting a sub-second cadence must run in their own session.
pub const SESSION_PARAMS: &[&str] = &["metrics_cadence"];

/// The session-grouping signature for a workload reference: its declared
/// values (following `extends:`) for the [`SESSION_PARAMS`], sorted.
/// Workloads with equal signatures can share one session; differing ones
/// must not. Empty signature = "the default session is fine".
pub fn session_param_signature(reference: &str) -> Vec<(String, String)> {
    let declared = nmbrs_workload::verify::declared_params(reference).unwrap_or_default();
    let mut sig: Vec<(String, String)> = SESSION_PARAMS
        .iter()
        .filter_map(|k| declared.get(*k).map(|v| ((*k).to_string(), v.clone())))
        .collect();
    sig.sort();
    sig
}

/// Resolve the metrics base interval + cadence ladder from the run's
/// effective params. A `metrics_cadence` param (workload or CLI, e.g.
/// `100ms` / `200ms`) sets the FINEST cadence — the pulse the SRD-86
/// optimizer settle detector samples — and the scheduler base interval, so
/// a windowed objective settles in a fraction of the default 1 s-cadence
/// wall-clock. A standard coarse ladder (1s/10s/30s/1m/5m) is layered above
/// it (keeping a 1 s rung bounds the fan-in from a sub-second floor). Absent
/// the param, this is the unchanged default: a 1 s base with the observer's
/// declared cadences (or [`Cadences::defaults`]).
fn resolve_cadence_config(
    params: &HashMap<String, String>,
    observer: &Arc<dyn crate::observer::RunObserver>,
) -> Result<(std::time::Duration, nmbrs_metrics::cadence::Cadences), String> {
    use std::time::Duration;
    let Some(raw) = params.get("metrics_cadence") else {
        let cadences = observer
            .cadences()
            .unwrap_or_else(nmbrs_metrics::cadence::Cadences::defaults);
        return Ok((Duration::from_secs(1), cadences));
    };
    let floor = nmbrs_metrics::cadence::parse_duration(raw).map_err(|_| {
        format!("metrics_cadence: invalid duration `{raw}` (use e.g. `100ms`, `200ms`, `1s`)")
    })?;
    if floor.is_zero() {
        return Err("metrics_cadence: must be greater than zero".to_string());
    }
    let mut layers = vec![floor];
    for secs in [1u64, 10, 30, 60, 300] {
        let d = Duration::from_secs(secs);
        if d > floor {
            layers.push(d);
        }
    }
    let cadences = nmbrs_metrics::cadence::Cadences::new(&layers)
        .map_err(|e| format!("metrics_cadence `{raw}`: cannot build a cadence ladder: {e:?}"))?;
    Ok((floor, cadences))
}

/// Collect every occurrence of a repeatable flag (e.g.
/// `--trace=<spec>` or `trace=<spec>`) from a raw arg list.
/// Returns the values in order of appearance — `parse_params`
/// collapses repeats into a HashMap, so this is the escape
/// hatch for repeatable args.
///
/// Accepts both `--name=value` and `name=value` shapes for
/// symmetry with the rest of nmbrs's arg surface.
pub fn collect_repeated_flag(args: &[String], name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut iter = args.iter().peekable();
    let long_eq = format!("--{name}=");
    let bare_eq = format!("{name}=");
    while let Some(arg) = iter.next() {
        let unquoted = elide_outer_quotes(arg.as_str());
        if let Some(v) = unquoted.strip_prefix(&long_eq) {
            out.push(elide_outer_quotes(v).to_string());
        } else if let Some(v) = unquoted.strip_prefix(&bare_eq) {
            out.push(elide_outer_quotes(v).to_string());
        } else if unquoted == format!("--{name}")
            && let Some(v) = iter.next()
        {
            out.push(elide_outer_quotes(v.as_str()).to_string());
        }
    }
    out
}

/// Parse a cycle count that may have suffixes: K, M, B.
pub fn parse_count(s: &str) -> Option<u64> {
    let s = s.trim().to_uppercase();
    if let Some(n) = s.strip_suffix('K') {
        n.trim().parse::<u64>().ok().map(|v| v * 1_000)
    } else if let Some(n) = s.strip_suffix('M') {
        n.trim().parse::<u64>().ok().map(|v| v * 1_000_000)
    } else if let Some(n) = s.strip_suffix('B') {
        n.trim().parse::<u64>().ok().map(|v| v * 1_000_000_000)
    } else {
        s.parse().ok()
    }
}

/// Find the closest match using Levenshtein distance.
fn closest_match<'a>(input: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let mut best: Option<(&str, usize)> = None;
    for &candidate in candidates {
        let d = levenshtein(input, candidate);
        if best.is_none() || d < best.unwrap().1 {
            best = Some((candidate, d));
        }
    }
    best.filter(|(_, d)| *d <= (input.len() / 2).max(2))
        .map(|(s, _)| s)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    let mut prev = (0..=n).collect::<Vec<_>>();
    let mut curr = vec![0; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

// =========================================================================
// Polydat Scope Composition (sysref 16)
// =========================================================================

// `ManifestEntry` and `extract_manifest` now live in
// `polydat::kernel`. Re-exported here so existing
// `crate::runner::extract_manifest` / `ManifestEntry` callers
// keep working — pure compatibility shim.
pub use polydat::kernel::{ManifestEntry, extract_manifest};

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_params quote elision (SRD 71) ──────────────────

    fn pp(args: &[&str]) -> HashMap<String, String> {
        parse_params(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parse_params_bare_unchanged() {
        let m = pp(&["cursor=0..53%"]);
        assert_eq!(m.get("cursor").map(String::as_str), Some("0..53%"));
    }

    #[test]
    fn parse_params_value_single_quoted_stripped() {
        let m = pp(&["cursor='0..53%'"]);
        assert_eq!(m.get("cursor").map(String::as_str), Some("0..53%"));
    }

    #[test]
    fn parse_params_value_double_quoted_stripped() {
        let m = pp(&["cursor=\"0..53%\""]);
        assert_eq!(m.get("cursor").map(String::as_str), Some("0..53%"));
    }

    #[test]
    fn parse_params_whole_arg_single_quoted_stripped() {
        let m = pp(&["'cursor=0..53%'"]);
        assert_eq!(m.get("cursor").map(String::as_str), Some("0..53%"));
    }

    #[test]
    fn parse_params_whole_arg_double_quoted_stripped() {
        let m = pp(&["\"cursor=0..53%\""]);
        assert_eq!(m.get("cursor").map(String::as_str), Some("0..53%"));
    }

    #[test]
    fn parse_params_bracket_value_with_quotes() {
        let m = pp(&["cursor='[0..53%)'"]);
        assert_eq!(m.get("cursor").map(String::as_str), Some("[0..53%)"));
    }

    #[test]
    fn parse_params_mismatched_quotes_not_stripped() {
        let m = pp(&["cursor='0..53%\""]);
        // First char `'`, last char `"` — no matching pair.
        assert_eq!(m.get("cursor").map(String::as_str), Some("'0..53%\""));
    }

    fn dup(args: &[&str]) -> Result<(), String> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        detect_conflicting_duplicate_params(&owned)
    }

    #[test]
    fn duplicate_conflicting_scenario_is_rejected() {
        let err = dup(&["workload=x.yaml", "scenario=reset", "scenario=idx_sweep"]).unwrap_err();
        assert!(
            err.contains("scenario") && err.contains("reset") && err.contains("idx_sweep"),
            "expected a conflicting-duplicate error naming both values, got: {err}"
        );
    }

    #[test]
    fn duplicate_identical_value_is_allowed() {
        // Re-passing the same value is harmless.
        assert!(dup(&["scenario=idx_sweep", "scenario=idx_sweep"]).is_ok());
    }

    #[test]
    fn distinct_params_are_allowed() {
        assert!(dup(&["workload=x.yaml", "scenario=reset", "cycles=10", "host=h"]).is_ok());
    }

    #[test]
    fn conflicting_duplicate_any_param_is_rejected() {
        // The guard is general — not just `scenario=`.
        assert!(dup(&["cycles=10", "cycles=20"]).is_err());
    }

    #[test]
    fn duplicate_check_elides_quotes_before_comparing() {
        // `scenario=reset` and `scenario='reset'` are the SAME value.
        assert!(dup(&["scenario=reset", "scenario='reset'"]).is_ok());
        // …but genuinely different quoted values still conflict.
        assert!(dup(&["scenario='reset'", "scenario=\"idx_sweep\""]).is_err());
    }

    #[test]
    fn duplicate_check_skips_session_flags_and_dotted_overrides() {
        // Session-dir flags are consumed by their own resolver; dotted
        // keys are phase-scoped overrides — neither participates here.
        assert!(dup(&["--session-path", "/a", "--session-path", "/b"]).is_ok());
        assert!(dup(&["phase1.cycles=10", "phase2.cycles=20"]).is_ok());
    }

    #[test]
    fn parse_params_equals_in_value_preserved() {
        // `key='a=b'` → name=`key`, value=`a=b`.
        let m = pp(&["key='a=b'"]);
        assert_eq!(m.get("key").map(String::as_str), Some("a=b"));
    }

    #[test]
    fn parse_params_multiple_params_independent() {
        let m = pp(&["dataset=example", "cursor='0..1%'", "concurrency=\"100\""]);
        assert_eq!(m.get("dataset").map(String::as_str), Some("example"));
        assert_eq!(m.get("cursor").map(String::as_str), Some("0..1%"));
        assert_eq!(m.get("concurrency").map(String::as_str), Some("100"));
    }

    // ── Polydat binding-LHS-name scanner ──────────────────────────

    fn scan_to_set(src: &str) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        scan_polydat_binding_lhs(&mut out, src);
        out
    }

    // ── scan_polydat_braced_refs: invalid `{...}` outside strings ─

    #[test]
    fn polydat_brace_guard_allows_valid_if_block() {
        // Block-form conditional selection uses braces legitimately. The guard must
        // not flag it, because the source parses.
        let src = "extern segments: u64 = 0\nmean := if segments > 0 { 100 } else { 0 }\n";
        let parses = polydat::dsl::lexer::lex(src)
            .ok()
            .and_then(|t| polydat::dsl::parser::parse(t).ok())
            .is_some();
        assert!(parses, "if-block source must parse: {src}");
    }

    #[test]
    fn polydat_brace_guard_still_catches_stray_placeholder() {
        // A YAML placeholder in expression position does NOT parse, so the guard
        // still fires and still names the placeholder.
        let src = "const passes := multiples_at_least({min_query_cycles}, base)\n";
        let parses = polydat::dsl::lexer::lex(src)
            .ok()
            .and_then(|t| polydat::dsl::parser::parse(t).ok())
            .is_some();
        assert!(!parses, "stray placeholder must fail to parse");
        let refs = scan_polydat_braced_refs(src);
        assert!(refs.iter().any(|r| r == "min_query_cycles"), "got {refs:?}");
    }

    #[test]
    fn scan_polydat_braced_refs_flags_expression_position_braces() {
        // The user's case: `{name}` outside any string literal,
        // sitting where Polydat expects an expression. Always invalid.
        let refs = scan_polydat_braced_refs(
            "const passes := multiples_at_least({min_query_cycles}, base)\n",
        );
        assert_eq!(refs, vec!["min_query_cycles".to_string()]);
    }

    #[test]
    fn scan_polydat_braced_refs_ignores_braces_inside_double_quotes() {
        // Inside `"…"` braces are valid — either workload-param
        // interp (if the runtime expanded the string earlier) or
        // Polydat string interpolation (parser turns it into printf).
        // Either way, scan_polydat_braced_refs must NOT flag them.
        let refs = scan_polydat_braced_refs(
            "const prebuffered := dataset_prebuffer(\"{dataset}:{profile}\")\n",
        );
        assert!(
            refs.is_empty(),
            "must not flag `{{dataset}}` / `{{profile}}` inside string \
             literal — string interpolation handles them, got {refs:?}"
        );
    }

    #[test]
    fn scan_polydat_braced_refs_ignores_braces_inside_single_quotes() {
        let refs = scan_polydat_braced_refs("tag := assert_eq(actual, '{expected}')\n");
        assert!(
            refs.is_empty(),
            "single-quoted strings get the same treatment: {refs:?}"
        );
    }

    #[test]
    fn scan_polydat_braced_refs_handles_escaped_quotes_in_strings() {
        // `"foo \"with brace {x}\" bar"` — the inner `{x}` is
        // inside a string the whole way through; backslash-escape
        // must not be treated as the end of the string.
        let refs = scan_polydat_braced_refs("x := concat(\"prefix \\\"{embedded}\\\" suffix\")\n");
        assert!(refs.is_empty(), "escaped quotes inside strings: {refs:?}");
    }

    #[test]
    fn scan_polydat_braced_refs_ignores_comments() {
        let refs = scan_polydat_braced_refs(
            "# this is a comment with {fake} placeholder\n\
             const real := 1\n",
        );
        assert!(
            refs.is_empty(),
            "`{{fake}}` inside a comment must not be flagged: {refs:?}"
        );
    }

    #[test]
    fn scan_polydat_braced_refs_catches_multiple_invalid_braces() {
        let refs = scan_polydat_braced_refs(
            "a := foo({x}, {y})\n\
             b := bar({z})\n",
        );
        // Order is source order; uniqueness isn't enforced here
        // (the validator caller dedups before reporting).
        assert_eq!(
            refs,
            vec!["x".to_string(), "y".to_string(), "z".to_string(),]
        );
    }

    #[test]
    fn scan_polydat_braced_refs_handles_mixed_string_and_expression_braces() {
        // `{inside}` is in a string (OK); `{outside}` is in
        // expression position (flagged).
        let refs = scan_polydat_braced_refs("x := concat(\"foo {inside}\", {outside})\n");
        assert_eq!(refs, vec!["outside".to_string()]);
    }

    // ── Polydat binding-LHS-name scanner ──────────────────────────

    #[test]
    fn scan_polydat_binding_lhs_handles_typed_cell_form() {
        // `shared name: type := default` — the type annotation sits
        // between the name and the assignment; the scanner must still
        // collect the name (undeclared-placeholder false-positive fix).
        let mut out = std::collections::HashSet::new();
        scan_polydat_binding_lhs(
            &mut out,
            "shared sstables: u64 := 0\nshared measured: f64 := 1.0\nplain := 2\n",
        );
        assert!(out.contains("sstables"), "{out:?}");
        assert!(out.contains("measured"), "{out:?}");
        assert!(out.contains("plain"), "{out:?}");
    }

    #[test]
    fn scan_polydat_binding_lhs_handles_tuple_destructure() {
        // Multi-output stdlib calls bind multiple names via
        // tuple destructure: `(a, b, c) := func(...)`. The
        // scanner must register every name on the LHS so the
        // placeholder validator doesn't false-flag downstream
        // `{a}` / `{b}` references.
        let names = scan_to_set("(y, mo, d, h, mi, s, ms) := date_components(0)\n");
        for expected in ["y", "mo", "d", "h", "mi", "s", "ms"] {
            assert!(
                names.contains(expected),
                "tuple-LHS scanner missed `{expected}` — got {names:?}"
            );
        }
    }

    #[test]
    fn scan_polydat_binding_lhs_finds_all_recognised_shapes() {
        // Validates the wire-name scanner picks up every shape:
        // init / cursor / shared / final modifier-prefixed
        // bindings, plus bare `NAME := …` assignments.
        let names = scan_to_set(
            "const prebuffered := dataset_prebuffer(\"foo\")\n\
             cursor q = range(0, 100)\n\
             query_vector := query_vector_at(prebuffered, q)\n\
             shared query_passes := set_or_get(query_passes, 7)\n\
             const tag := \"label_00\"\n",
        );
        for expected in ["prebuffered", "q", "query_vector", "query_passes", "tag"] {
            assert!(
                names.contains(expected),
                "scanner missed `{expected}` — got {names:?}"
            );
        }
    }

    #[test]
    fn scan_polydat_binding_lhs_skips_comments_and_blank_lines() {
        let names = scan_to_set(
            "# comment\n\
             \n\
             const real_binding := 1\n\
             # another comment\n",
        );
        assert_eq!(names.len(), 1);
        assert!(names.contains("real_binding"));
    }

    #[test]
    fn scan_polydat_binding_lhs_ignores_non_binding_lines() {
        // Expression-call statements and continuation lines must
        // not introduce phantom wires.
        let names = scan_to_set(
            "foo(1, 2)\n\
             bar.baz\n\
             const real := 1\n",
        );
        assert_eq!(names.len(), 1);
        assert!(names.contains("real"));
    }

    #[test]
    fn scan_polydat_binding_lhs_picks_up_input_decl_bare() {
        let names = scan_to_set("input cycle: u64\nx := hash(cycle)\n");
        assert!(names.contains("cycle"));
        assert!(names.contains("x"));
    }

    #[test]
    fn scan_polydat_binding_lhs_picks_up_input_decl_untyped() {
        let names = scan_to_set("input cycle\n");
        assert!(names.contains("cycle"));
    }

    #[test]
    fn scan_polydat_binding_lhs_picks_up_input_decl_tuple() {
        let names = scan_to_set("input (cycle: u64, q: f64)\n");
        assert!(names.contains("cycle"));
        assert!(names.contains("q"));
    }

    #[test]
    fn scan_polydat_binding_lhs_picks_up_extern_decl() {
        // `extern name: type [= default]` declares a wire just like
        // `input` — a same-op capture target referenced via `{name}`
        // must not trip the undeclared-placeholder guard.
        let names = scan_to_set(
            "extern active_compactions: u64 = 0\n\
             extern completion_ratio: f64 = 0.0\n\
             extern (a: u64, b: f64)\n",
        );
        assert!(names.contains("active_compactions"));
        assert!(names.contains("completion_ratio"));
        assert!(names.contains("a"));
        assert!(names.contains("b"));
    }

    #[test]
    fn parse_dryrun_controls_sets_list_flag() {
        let cfg = DiagnosticConfig::parse("controls");
        assert!(cfg.list_controls);
        // Implies phase depth so the runner exits before any
        // cycle-time work.
        assert_eq!(cfg.depth, ExecDepth::Phase);
    }

    #[test]
    fn parse_dryrun_controls_combines_with_other_flags() {
        let cfg = DiagnosticConfig::parse("controls,labels");
        assert!(cfg.list_controls);
        assert!(cfg.show_labels);
    }

    #[test]
    fn parse_dryrun_unknown_flag_does_not_set_controls() {
        let cfg = DiagnosticConfig::parse("phase,bogus");
        assert!(!cfg.list_controls);
    }

    // ── SRD-13d Phase 7 — `dryrun=op` ──

    #[test]
    fn parse_dryrun_op_sets_op_depth() {
        let cfg = DiagnosticConfig::parse("op");
        assert_eq!(cfg.depth, ExecDepth::Op);
    }

    #[test]
    fn parse_dryrun_phase_still_sets_phase_depth() {
        let cfg = DiagnosticConfig::parse("phase");
        assert_eq!(cfg.depth, ExecDepth::Phase);
    }

    #[test]
    fn parse_dryrun_cycle_still_sets_cycle_depth() {
        let cfg = DiagnosticConfig::parse("cycle");
        assert_eq!(cfg.depth, ExecDepth::Cycle);
    }

    #[test]
    fn parse_dryrun_op_combines_with_wiring_flag() {
        let cfg = DiagnosticConfig::parse("op,wiring");
        assert_eq!(cfg.depth, ExecDepth::Op);
        assert!(cfg.show_wiring);
    }

    #[test]
    fn parse_dryrun_wiring_alone_bumps_depth_to_op() {
        // `wiring` needs depth >= Op for kernels to exist; a bare
        // `dryrun=wiring` must auto-bump so it produces output
        // instead of silently doing nothing at depth=Phase.
        let cfg = DiagnosticConfig::parse("wiring");
        assert_eq!(cfg.depth, ExecDepth::Op);
        assert!(cfg.show_wiring);
    }

    #[test]
    fn parse_dryrun_wiring_does_not_override_explicit_depth() {
        // Explicit phase depth wins; `wiring` is then a no-op
        // (no kernels to render at phase depth) — the user gets
        // what they asked for rather than a silent bump.
        let cfg = DiagnosticConfig::parse("phase,wiring");
        assert_eq!(cfg.depth, ExecDepth::Phase);
        assert!(cfg.show_wiring);
    }

    #[test]
    fn exec_depth_ordering_matches_srd_13d() {
        // `Phase` is the shallowest stop, `Full` is the deepest;
        // `Op` sits between `Phase` and `Cycle`. Depth-gating
        // sites read this ordering as `< Cycle` ⇒ "skip cycles".
        assert!(ExecDepth::Phase < ExecDepth::Op);
        assert!(ExecDepth::Op < ExecDepth::Cycle);
        assert!(ExecDepth::Cycle < ExecDepth::Full);
        // The transitive should hold (it would be a derive
        // bug if it didn't, but assert it for documentation).
        assert!(ExecDepth::Phase < ExecDepth::Cycle);
        assert!(ExecDepth::Op < ExecDepth::Full);
    }

    #[test]
    fn exec_depth_phase_and_op_short_circuit_before_cycles() {
        // The executor's per-phase early-exit fires when
        // `depth < Cycle`. Both Phase and Op satisfy that;
        // Cycle and Full do not.
        assert!(ExecDepth::Phase < ExecDepth::Cycle);
        assert!(ExecDepth::Op < ExecDepth::Cycle);
        assert!((ExecDepth::Cycle >= ExecDepth::Cycle));
        assert!((ExecDepth::Full >= ExecDepth::Cycle));
    }

    #[test]
    fn render_scope_elision_summary_shows_materialised_and_elides_to() {
        use nmbrs_workload::model::{BindingsDef, ScenarioNode, WorkloadPhase};
        use std::collections::HashMap;

        let phase = WorkloadPhase {
            key_metrics: Vec::new(),
            dimensions: Default::default(),
            cycles: None,
            concurrency: None,
            rate: None,
            daemon: false,
            adapter: None,
            errors: None,
            tries: None,
            tries_backoff: None,
            interval: None,
            repeat: None,
            error_rate_max: None,
            timeout: None,
            stop_when: Vec::new(),
            throttle: None,
            continue_if: None,
            tags: None,
            ops: vec![],
            for_each: None,
            loop_scope: None,
            iter_scope: None,
            checkpoint: None,
            status_metrics: vec![],
            metrics: Default::default(),
            bindings: BindingsDef::default(),
            poll: None,
            optimize: None,
        };
        let mut phases = HashMap::new();
        phases.insert("predict".to_string(), phase);
        let mut tree = crate::scope_tree::ScopeTree::build(
            "default",
            &[ScenarioNode::Phase("predict".into())],
        );
        // Conservative classifier: empty workload + empty
        // phase ⇒ scenario and phase elide into root.
        let inputs = crate::scope_elision::ClassifyInputs {
            bindings: &BindingsDef::default(),
            params: &HashMap::new(),
            phases: &phases,
        };
        crate::scope_elision::classify_and_mark(&mut tree, &inputs);

        let mut buf: Vec<u8> = Vec::new();
        render_scope_elision_summary(&tree, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();

        assert!(s.contains("scope elision summary"), "missing header: {s}");
        // Workload root materialises always (SRD-13d §5.1).
        assert!(
            s.contains("workload") && s.contains("materialised=true"),
            "expected materialised=true line for workload root: {s}"
        );
        // Scenario + phase elide into the workload root.
        assert!(
            s.contains("elides-to=workload"),
            "expected elides-to=workload for empty phase: {s}"
        );
        assert!(
            s.contains("workload.scenario.default"),
            "expected scenario logical name: {s}"
        );
        assert!(
            s.contains("workload.scenario.default.phase.predict"),
            "expected phase logical name: {s}"
        );
    }

    #[test]
    fn render_controls_tree_empty_session_writes_placeholder() {
        let root = nmbrs_metrics::component::Component::root(
            nmbrs_metrics::labels::Labels::of("session", "t"),
            std::collections::HashMap::new(),
        );
        let mut buf: Vec<u8> = Vec::new();
        render_controls_tree(&root, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("no controls declared"), "got: {s}");
    }

    #[test]
    fn render_controls_tree_lists_session_root_controls() {
        let root = nmbrs_metrics::component::Component::root(
            nmbrs_metrics::labels::Labels::of("session", "t"),
            std::collections::HashMap::new(),
        );
        root.read().unwrap().controls().declare(
            nmbrs_metrics::controls::ControlBuilder::new("log_level", 1u32)
                .reify_as_gauge(|v| Some(*v as f64))
                .branch_scope(nmbrs_metrics::controls::BranchScope::Subtree)
                .from_f64(|v| Ok(v as u32))
                .final_at_scope("session_root")
                .build(),
        );

        let mut buf: Vec<u8> = Vec::new();
        render_controls_tree(&root, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("log_level"), "missing name: {s}");
        assert!(s.contains("scope=subtree"), "missing scope: {s}");
        assert!(
            s.contains("final@session_root"),
            "missing final marker: {s}"
        );
        assert!(s.contains("f64-writable"), "missing write surface: {s}");
    }

    // ── Regression: --session-path value not auto-promoted to scenario= ──
    //
    // Bug shape (caught by user during Phase C live exercise):
    // `nmbrs run wl.yaml cycles=2 --session-path X` was rewritten to
    // `nmbrs run wl.yaml cycles=2 --session-path scenario=X` because
    // `normalize_args` walked tokens flat and saw `X` as a bare
    // post-workload positional. Symptom: a literal directory at
    // `<cwd>/scenario=X` was created. The fix peeks for value-taking
    // flags so the value passes through unchanged.

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn normalize_args_session_path_space_form_value_passes_through() {
        let out = normalize_args(&s(&[
            "wl.yaml",
            "cycles=2",
            "--session-path",
            "target/test-tmp/foo/session",
        ]));
        // The path arg must NOT be turned into `scenario=...`.
        assert!(
            !out.iter().any(|a| a.starts_with("scenario=")),
            "scenario= auto-promotion fired on a flag value: {out:?}"
        );
        assert_eq!(
            out,
            s(&[
                "wl.yaml",
                "cycles=2",
                "--session-path",
                "target/test-tmp/foo/session",
            ])
        );
    }

    #[test]
    fn normalize_args_session_path_equals_form_unchanged() {
        let out = normalize_args(&s(&[
            "wl.yaml",
            "--session-path=target/test-tmp/foo/session",
        ]));
        assert_eq!(
            out,
            s(&["wl.yaml", "--session-path=target/test-tmp/foo/session",])
        );
    }

    #[test]
    fn normalize_args_real_scenario_positional_still_promotes() {
        // The original feature: bare-word scenario shorthand.
        // Must keep working when no value-flag interferes.
        let out = normalize_args(&s(&["wl.yaml", "myscenario", "cycles=2"]));
        assert_eq!(out, s(&["wl.yaml", "scenario=myscenario", "cycles=2",]));
    }

    #[test]
    fn normalize_args_scenario_after_session_path_still_promotes() {
        // After a value-flag pair, the next free positional is
        // still eligible for scenario= promotion. This confirms
        // the bookkeeping survives the look-ahead.
        let out = normalize_args(&s(&["wl.yaml", "--session-path", "/tmp/x", "myscenario"]));
        assert_eq!(
            out,
            s(&["wl.yaml", "--session-path", "/tmp/x", "scenario=myscenario",])
        );
    }

    #[test]
    fn normalize_args_readout_value_passes_through() {
        let out = normalize_args(&s(&["wl.yaml", "--readout", "throughput ok_pct"]));
        assert!(
            !out.iter().any(|a| a.starts_with("scenario=")),
            "readout body misread as scenario: {out:?}"
        );
    }

    /// `format_for_combinations` lays out vars and specs in
    /// column-aligned pairs. Each column is padded to the
    /// widest of its (var, spec) so corresponding entries
    /// stack vertically. The `color = false` argument forces
    /// the no-ANSI branch so the assertion can pattern-match
    /// the raw text — no dependency on the process's ambient
    /// TTY / `NO_COLOR` state (which `observer::use_color()`
    /// caches process-wide on first call and can't be undone
    /// per-test).
    #[test]
    fn format_for_combinations_aligns_columns() {
        let pairs = vec![
            ("sm".to_string(), "{sm_values}".to_string()),
            ("mnc".to_string(), "{mnc_values}".to_string()),
            (
                "alf_label".to_string(),
                "concat({alf_label_values})".to_string(),
            ),
        ];
        let out = format_for_combinations(&pairs, "", false);
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines.len(), 2, "MUST produce exactly 2 lines: {out:?}");
        assert!(
            lines[0].starts_with("for ["),
            "first line MUST start with `for [`: {:?}",
            lines[0]
        );
        assert!(
            lines[1].starts_with(" in ["),
            "second line MUST start with ` in [`: {:?}",
            lines[1]
        );
        // Bracket columns align: the `[` after `for` and the
        // `[` after `in ` should be at the same column index.
        let l0_bracket = lines[0].find('[').unwrap();
        let l1_bracket = lines[1].find('[').unwrap();
        assert_eq!(
            l0_bracket, l1_bracket,
            "`[` brackets MUST align: line0={l0_bracket}, line1={l1_bracket}"
        );
        // Column alignment: the comma after `sm,` on line 0
        // sits at the same column as the comma after the
        // `{sm_values},` on line 1 — except padded so that
        // `mnc` on line 0 starts at the same column as
        // `{mnc_values}` on line 1.
        let mnc_pos = lines[0].find("mnc").unwrap();
        let mnc_values_pos = lines[1].find("{mnc_values}").unwrap();
        assert_eq!(
            mnc_pos, mnc_values_pos,
            "column 2 MUST align: `mnc`@{mnc_pos} vs `{{mnc_values}}`@{mnc_values_pos}\n{out}"
        );
        let alf_pos = lines[0].find("alf_label").unwrap();
        let alf_concat_pos = lines[1].find("concat(").unwrap();
        assert_eq!(
            alf_pos, alf_concat_pos,
            "column 3 MUST align: `alf_label`@{alf_pos} vs `concat(...)`@{alf_concat_pos}\n{out}"
        );
        // Closing brackets present on both lines.
        assert!(lines[0].ends_with(']'));
        assert!(lines[1].ends_with(']'));
    }
}
