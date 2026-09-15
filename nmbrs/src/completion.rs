// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

// Module-level allow: this file holds the legacy shell-completion
// builder API (`build_tree`, the `*_node` family, `Category` /
// `Level` enums duplicated in `cli_spec`). Retained per the doc
// comment on `build_tree`: "main.rs uses
// `cli_spec::completion::build_command_tree(&root)` instead, so
// the spec is the single source of truth." The orphan tree
// stays compileable as a fallback path for downstream tooling
// that hasn't migrated to the spec-driven builder yet.
#![allow(dead_code)]

//! Stratified shell completion for the `nmbrs` CLI, built on
//! [`veks_completion`].
//!
//! ## Tap progression
//!
//! Three tap tiers, gated by per-command metadata (see SRD-15 if
//! ever written; for now the contract is encoded in this file).
//!
//! - **Tap 1** — primary commands the user reaches for daily:
//!   `run` (start a workload) and `attach` (connect to a
//!   running one over the OOB socket).
//! - **Tap 2** — adds secondary commands (`summary`).
//! - **Tap 3** — full surface (subcommands like `describe`,
//!   `bench`, `plot`, `web`, `completions`).
//!
//! Categories are a closed set defined by the [`Category`] enum
//! (which implements [`veks_completion::CategoryTag`]); tap tiers
//! are likewise a closed set defined by the [`Level`] enum
//! (implementing [`veks_completion::LevelTag`]). Renderers can
//! group commands by `Category::tag()` and order by
//! `Level::rank()`.
//!
//! The tree is built in [`build_tree`] using
//! [`veks_completion::CommandTree::strict_command`], which
//! requires every node to declare both a category and a level
//! at the **type** level — undertagged commands fail to compile.

use veks_completion::{CategoryTag, CommandTree, LevelTag, Node, StrictNode, fn_provider};

use nmbrs_runtime::adapter::registered_driver_names;
use nmbrs_runtime::runner::{resolve_workload_file_public, scenarios_in_workload_file};

// ---------------------------------------------------------------------------
// Categories — closed enum implementing veks_completion::CategoryTag so the
// set of valid categories is defined once and the compiler enforces variants
// rather than a scattered constellation of `&str` constants. Renderers can
// group commands by `tag()` (the stable lowercase key).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum Category {
    Workloads,
    Tools,
    Documentation,
    Benchmark,
    Server,
    Shell,
}

impl CategoryTag for Category {
    fn tag(&self) -> &'static str {
        match self {
            Category::Workloads => "workloads",
            Category::Tools => "tools",
            Category::Documentation => "documentation",
            Category::Benchmark => "benchmark",
            Category::Server => "server",
            Category::Shell => "shell",
        }
    }
}

// ---------------------------------------------------------------------------
// Tap tiers — closed enum implementing veks_completion::LevelTag. The Nth
// tab tap reveals every root command with `rank() <= N`. Naming the tiers
// keeps build_tree() self-describing instead of bare 1/2/3 sprinkled through.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum Level {
    /// Tap 1 — primary commands the user reaches for daily
    /// (`run`, `shell`).
    Workload,
    /// Tap 2 — secondary commands (`summary`).
    Secondary,
    /// Tap 3 — the full subcommand surface (describe, bench,
    /// plot, …).
    FullSurface,
}

impl LevelTag for Level {
    fn rank(&self) -> u32 {
        match self {
            Level::Workload => 1,
            Level::Secondary => 2,
            Level::FullSurface => 3,
        }
    }
    fn name(&self) -> &'static str {
        match self {
            Level::Workload => "workload",
            Level::Secondary => "secondary",
            Level::FullSurface => "full-surface",
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Build the full nmbrs completion tree. Strictly typed —
/// every command must declare both a category and a tap level
/// or the build fails to compile (`StrictNode<true, true>`
/// gating on `strict_command`).
pub fn build_tree() -> CommandTree {
    let root = crate::cli_spec::root::root();
    crate::cli_spec::completion::build_command_tree(&root)
}

/// Every `key=value` param the run-style grammar (`run`,
/// `refine`) accepts — the single source of truth for that
/// vocabulary. The runner's param validation derives its
/// allow-list from this list at startup (via [`known_param_keys`]
/// → `nmbrs_runtime::runner::install_known_params`), so there is no
/// hand-synced copy to drift. Each key carries a completion
/// provider. veks-completion 1.3.1 removed tree-global
/// providers — value completion is per-node — so these are
/// declared on the commands that accept them via
/// [`crate::cli_spec::Command::kv_params`]. Keys whose value
/// space is free-form (cycles, rate, tags, …) carry
/// [`free_form`] so they still appear as option tokens.
pub static RUN_KV_PARAMS: &[crate::cli_spec::KvParam] = &[
    WORKLOAD_KV_PARAM,
    // SRD-108 — the implementation module bound into a logical
    // workload's abstract op slots.
    crate::cli_spec::KvParam {
        key: "impl=",
        provider: workload_provider,
    },
    crate::cli_spec::KvParam {
        key: "scenario=",
        provider: scenario_provider,
    },
    ADAPTER_KV_PARAM,
    DRIVER_KV_PARAM,
    crate::cli_spec::KvParam {
        key: "profiler=",
        provider: static_profiler,
    },
    crate::cli_spec::KvParam {
        key: "tui=",
        provider: static_tui,
    },
    crate::cli_spec::KvParam {
        key: "format=",
        provider: static_stdout_format,
    },
    crate::cli_spec::KvParam {
        key: "dryrun=",
        provider: static_dryrun,
    },
    crate::cli_spec::KvParam {
        key: "sysmon=",
        provider: static_sysmon,
    },
    SYSMON_INTERVAL_KV_PARAM,
    crate::cli_spec::KvParam {
        key: "watch=",
        provider: static_watch,
    },
    crate::cli_spec::KvParam {
        key: "scope=",
        provider: static_scope,
    },
    crate::cli_spec::KvParam {
        key: "on_removed=",
        provider: static_on_removed,
    },
    SEQ_KV_PARAM,
    crate::cli_spec::KvParam {
        key: "kernel_opt=",
        provider: static_kernel_opt,
    },
    crate::cli_spec::KvParam {
        key: "skipped_phases=",
        provider: static_skipped_phases,
    },
    crate::cli_spec::KvParam {
        key: "completed_phases=",
        provider: static_completed_phases,
    },
    crate::cli_spec::KvParam {
        key: "phases=",
        provider: workload_phase_provider,
    },
    crate::cli_spec::KvParam {
        key: "resume=",
        provider: session_name_provider,
    },
    crate::cli_spec::KvParam {
        key: "header=",
        provider: bool_values,
    },
    crate::cli_spec::KvParam {
        key: "color=",
        provider: bool_values,
    },
    crate::cli_spec::KvParam {
        key: "inspector=",
        provider: bool_values,
    },
    crate::cli_spec::KvParam {
        key: "resume_latest=",
        provider: bool_values,
    },
    // SRD-106 — override the workload's `stick_session:` declaration
    // from the CLI (`true` forces stick on, `false` disables it).
    crate::cli_spec::KvParam {
        key: "stick_session=",
        provider: bool_values,
    },
    crate::cli_spec::KvParam {
        key: "force_retry_failed=",
        provider: bool_values,
    },
    crate::cli_spec::KvParam {
        key: "profiler_callgraph=",
        provider: bool_values,
    },
    crate::cli_spec::KvParam {
        key: "schedule=",
        provider: static_schedule,
    },
    // Free-form value spaces — listed so the option completes,
    // value typed freely.
    crate::cli_spec::KvParam {
        key: "op=",
        provider: free_form,
    },
    CYCLES_KV_PARAM,
    crate::cli_spec::KvParam {
        key: "concurrency=",
        provider: free_form,
    },
    RATE_KV_PARAM,
    ERRORS_KV_PARAM,
    // SRD-82 Part 3b — workload-root total-attempts budget (the `tries`
    // sigil for the conditional tries wrapper). Absent → single attempt.
    crate::cli_spec::KvParam {
        key: "tries=",
        provider: free_form,
    },
    crate::cli_spec::KvParam {
        key: "error_rate_max=",
        provider: free_form,
    },
    crate::cli_spec::KvParam {
        key: "tags=",
        provider: free_form,
    },
    FILENAME_KV_PARAM,
    crate::cli_spec::KvParam {
        key: "separator=",
        provider: free_form,
    },
    STANZA_CONCURRENCY_KV_PARAM,
    crate::cli_spec::KvParam {
        key: "sc=",
        provider: free_form,
    },
    crate::cli_spec::KvParam {
        key: "summary=",
        provider: free_form,
    },
    crate::cli_spec::KvParam {
        key: "metrics=",
        provider: free_form,
    },
    crate::cli_spec::KvParam {
        key: "limit=",
        provider: free_form,
    },
    crate::cli_spec::KvParam {
        key: "latency_cadences=",
        provider: free_form,
    },
    crate::cli_spec::KvParam {
        key: "jobname=",
        provider: free_form,
    },
    crate::cli_spec::KvParam {
        key: "instance=",
        provider: free_form,
    },
    crate::cli_spec::KvParam {
        key: "prompush_apikeyfile=",
        provider: free_form,
    },
    crate::cli_spec::KvParam {
        key: "trace=",
        provider: free_form,
    },
    // Log-level floors (SRD-41) — accepted in both `-` and `_`
    // spellings; `loglevel`/`loglevel-display` set the display
    // (stderr) floor, `loglevel-retain` the session.log floor.
    crate::cli_spec::KvParam {
        key: "loglevel=",
        provider: free_form,
    },
    crate::cli_spec::KvParam {
        key: "loglevel-display=",
        provider: free_form,
    },
    crate::cli_spec::KvParam {
        key: "loglevel_display=",
        provider: free_form,
    },
    crate::cli_spec::KvParam {
        key: "loglevel-retain=",
        provider: free_form,
    },
    crate::cli_spec::KvParam {
        key: "loglevel_retain=",
        provider: free_form,
    },
    // `latency-cadences=` dash spelling (the `_` form is above).
    crate::cli_spec::KvParam {
        key: "latency-cadences=",
        provider: free_form,
    },
    // `scenarios=` — multi-scenario selector.
    crate::cli_spec::KvParam {
        key: "scenarios=",
        provider: scenario_provider,
    },
    // SRD-86 — finest metrics cadence + scheduler base interval
    // (e.g. `100ms`/`200ms`), so a windowed optimizer objective
    // settles in a fraction of the default 1s-cadence wall-clock.
    crate::cli_spec::KvParam {
        key: "metrics_cadence=",
        provider: free_form,
    },
    // SRD-91 — op-outcome instrument detail. `counts` or `timers`
    // (global default), with optional per-family overrides in one value:
    // `metrics_detail=timers,attempt_success:counts,attempt_failure:counts`.
    crate::cli_spec::KvParam {
        key: "metrics_detail=",
        provider: free_form,
    },
    // Metrics output sinks. Both have a `--flag` spelling too; they are listed
    // here because the runner reads them from params as well
    // (`params.get("metrics-log")` / `params.get("per-instance-metrics")`), and
    // this list IS the known-param allow-list — an unlisted key that the runner
    // nonetheless honours gets reported as unknown, which is the worst of both.
    crate::cli_spec::KvParam {
        key: "metrics-log=",
        provider: free_form,
    },
    crate::cli_spec::KvParam {
        key: "per-instance-metrics=",
        provider: bool_values,
    },
    // Bare `session=<path|name>`, the sibling of `--session`. Accepted by
    // `resolve_session_dir` for every command, run included, so it must be a
    // known param here or a run that honours it also warns that it is unknown.
    SESSION_KV_PARAM,
    // The runner reads a bare `report-openmetrics-to=<url>` beside the
    // `--report-openmetrics-to` flag spelling, so it is a known param too.
    crate::cli_spec::KvParam {
        key: "report-openmetrics-to=",
        provider: free_form,
    },
];

/// The run-style `key=value` param vocabulary (each key sans its trailing
/// `=`), derived from [`RUN_KV_PARAMS`] — the single source of truth for
/// "what params does a run accept". Installed into the runner at startup
/// (`nmbrs_runtime::runner::install_known_params`) so workload/CLI param
/// validation references this same CLI command-spec instead of a
/// hand-synced copy. Adding a param to `RUN_KV_PARAMS` makes it complete
/// AND validate with no second edit.
pub fn known_param_keys() -> Vec<&'static str> {
    RUN_KV_PARAMS
        .iter()
        .map(|kv| kv.key.trim_end_matches('='))
        .collect()
}

// ── Closed-set / dynamic value providers (audit 2026-06-11) ──

/// Free-form value space: the key completes as an option, the
/// value is typed freely.
pub(crate) fn free_form(_partial: &str, _ctx: &[&str]) -> Vec<String> {
    Vec::new()
}

pub(crate) fn bool_values(partial: &str, _ctx: &[&str]) -> Vec<String> {
    filter_prefix(&["true", "false"], partial)
}

fn static_seq(partial: &str, _ctx: &[&str]) -> Vec<String> {
    filter_prefix(&["bucket", "interval", "concat"], partial)
}

pub(crate) fn static_kernel_opt(partial: &str, _ctx: &[&str]) -> Vec<String> {
    filter_prefix(&["release", "diagnostic"], partial)
}

fn static_skipped_phases(partial: &str, _ctx: &[&str]) -> Vec<String> {
    filter_prefix(&["elide", "mark", "prune"], partial)
}

fn static_completed_phases(partial: &str, _ctx: &[&str]) -> Vec<String> {
    filter_prefix(&["full", "headers"], partial)
}

fn static_schedule(partial: &str, _ctx: &[&str]) -> Vec<String> {
    filter_prefix(&["*"], partial)
}

pub(crate) fn session_reuse_values(partial: &str, _ctx: &[&str]) -> Vec<String> {
    filter_prefix(&["error", "restart", "resume"], partial)
}

/// Existing session names: directory names under `./logs`
/// (the SRD-04 session umbrella), `latest` included — it is a
/// valid `--session` target.
pub(crate) fn session_name_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    // Ask the runtime where sessions live rather than hardcoding a directory.
    // This read `"logs"` unconditionally, which SRD-77 renamed to `sessions/`,
    // so `--session <TAB>` silently suggested nothing on every current layout.
    // `logs/` is still scanned second for a pre-SRD-77 tree.
    let mut roots = vec![nmbrs_runtime::session::default_sessions_root()];
    roots.push(std::path::PathBuf::from("logs"));
    let mut out: Vec<String> = Vec::new();
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        out.extend(
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with(partial)),
        );
    }
    out.sort();
    out.dedup();
    out
}

/// Distinct phase identities from the latest session's
/// `phase_outcomes` table (SRD-76). Best-effort, same
/// convention as [`execution_id_provider`].
pub(crate) fn phase_name_db_provider(partial: &str, ctx: &[&str]) -> Vec<String> {
    // Resolve the db from the command line — `--db`, the `--session*` family,
    // or a bare `session=` — instead of assuming the latest session. Hardcoding
    // `latest_metrics_db()` offered values from a DIFFERENT session than the one
    // being targeted: with `--session=<dir>` naming a session holding only
    // exec 77, completion still offered `0 1` from `sessions/latest`. Values that
    // do not exist in the named session are worse than no suggestion at all.
    let db_path = db_path_from_context(ctx);
    if !db_path.exists() {
        return Vec::new();
    }
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return Vec::new();
    };
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
         WHERE type='table' AND name='phase_outcomes')",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n != 0)
        .unwrap_or(false);
    if !exists {
        return Vec::new();
    }
    let Ok(mut stmt) =
        conn.prepare("SELECT DISTINCT phase_name FROM phase_outcomes ORDER BY phase_name")
    else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else {
        return Vec::new();
    };
    rows.filter_map(|r| r.ok())
        .filter(|n| n.starts_with(partial))
        .collect()
}

/// `nmbrs metrics query --range <TAB>` — completes to `all` plus the
/// session's ACTUAL reporting cadences, read live from the data (the
/// distinct `interval_ms` windows in `sample_value`). `all` expands to the
/// whole test span; each cadence is the normative metricsql `[<dur>]`
/// range-vector window.
pub(crate) fn metrics_range_provider(partial: &str, ctx: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = vec!["all".to_string()];
    // Resolve the db from the command line — `--db`, the `--session*` family,
    // or a bare `session=` — instead of assuming the latest session. Hardcoding
    // `latest_metrics_db()` offered values from a DIFFERENT session than the one
    // being targeted: with `--session=<dir>` naming a session holding only
    // exec 77, completion still offered `0 1` from `sessions/latest`. Values that
    // do not exist in the named session are worse than no suggestion at all.
    let db_path = db_path_from_context(ctx);
    if db_path.exists()
        && let Ok(conn) = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
    {
        // `prepare` fails cleanly if `sample_value` is absent → just `all`.
        if let Ok(mut stmt) = conn.prepare(
            "SELECT DISTINCT interval_ms FROM sample_value \
             WHERE interval_ms > 0 ORDER BY interval_ms",
        ) && let Ok(rows) = stmt.query_map([], |r| r.get::<_, i64>(0))
        {
            for ms in rows.filter_map(|r| r.ok()) {
                out.push(format_duration_ms(ms));
            }
        }
    }
    out.into_iter().filter(|v| v.starts_with(partial)).collect()
}

/// Format a millisecond window as a compact metricsql duration:
/// `1000 → "1s"`, `60000 → "1m"`, else `"<ms>ms"`.
fn format_duration_ms(ms: i64) -> String {
    if ms % 86_400_000 == 0 {
        format!("{}d", ms / 86_400_000)
    } else if ms % 3_600_000 == 0 {
        format!("{}h", ms / 3_600_000)
    } else if ms % 60_000 == 0 {
        format!("{}m", ms / 60_000)
    } else if ms % 1_000 == 0 {
        format!("{}s", ms / 1_000)
    } else {
        format!("{ms}ms")
    }
}

/// `phases=` filter completion: phase names declared by the
/// `workload=` already on the line (falls back to the latest
/// session db when no workload is in context).
fn workload_phase_provider(partial: &str, ctx: &[&str]) -> Vec<String> {
    for word in ctx {
        if let Some(name) = word.strip_prefix("workload=") {
            if let Some(text) = workload_text_for_completion(name)
                && let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(&text)
                && let Some(phases) = doc.get("phases").and_then(|v| v.as_mapping())
            {
                let mut out: Vec<String> = phases
                    .keys()
                    .filter_map(|k| k.as_str())
                    .filter(|n| n.starts_with(partial))
                    .map(|n| n.to_string())
                    .collect();
                out.sort();
                return out;
            }
            return Vec::new();
        }
    }
    phase_name_db_provider(partial, ctx)
}

/// Inspector one-shot command names — the same canonical set
/// the unix-socket server dispatches on.
pub(crate) fn inspector_command_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    nmbrs_tui::inspector_server::COMMAND_NAMES
        .iter()
        .filter(|c| c.starts_with(partial))
        .map(|c| c.to_string())
        .collect()
}

/// Bind-address suggestions for `nmbrs web --bind`.
pub(crate) fn bind_addr_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    filter_prefix(&["127.0.0.1", "0.0.0.0", "localhost", "::1"], partial)
}

/// Bundled catalog names (both tiers) — `nmbrs copy <TAB>`.
pub(crate) fn catalog_name_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    nmbrs_workload::catalog::iter()
        .map(|w| w.name.to_string())
        .filter(|n| n.starts_with(partial))
        .collect()
}

/// `nmbrs describe workloads <TAB>`: catalog names plus the
/// `examples` tier selector.
pub(crate) fn describe_workloads_arg_provider(partial: &str, ctx: &[&str]) -> Vec<String> {
    let mut out = catalog_name_provider(partial, ctx);
    if "examples".starts_with(partial) {
        out.push("examples".to_string());
    }
    out.sort();
    out
}

/// Directory-only completion (e.g. `nmbrs diag query-labels
/// <dbdir>`). Walks one level from the partial's parent.
pub(crate) fn dirs_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    let (dir, name_prefix) = match partial.rfind('/') {
        Some(i) => (&partial[..=i], &partial[i + 1..]),
        None => ("./", partial),
    };
    let Ok(rd) = std::fs::read_dir(if dir.is_empty() { "./" } else { dir }) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with(name_prefix) && !n.starts_with('.'))
        .map(|n| {
            let base = if dir == "./" && !partial.starts_with("./") {
                ""
            } else {
                dir
            };
            format!("{base}{n}/")
        })
        .collect();
    out.sort();
    out
}

/// Metric-family completion for `nmbrs metrics list/show/groups
/// <expr>` positionals.
pub(crate) fn metric_family_provider(partial: &str, ctx: &[&str]) -> Vec<String> {
    metric_provider(partial, ctx)
}

/// Registered adapter names — `nmbrs describe adapter <TAB>`.
pub(crate) fn adapter_names_provider(partial: &str, ctx: &[&str]) -> Vec<String> {
    adapter_provider(partial, ctx)
}

/// Workload positional (file or catalog name) — `nmbrs describe
/// op <workload> …`.
/// Dynamic-control names — `nmbrs describe controls <TAB>`. Drawn from the
/// static capability catalog (SRD-23) so every control the binary can declare
/// is completable, conditional ones included.
pub(crate) fn control_names_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    let names: Vec<&str> = nmbrs_runtime::control_catalog::all_controls()
        .iter()
        .map(|e| e.desc.name)
        .collect();
    filter_prefix(&names, partial)
}

/// Benchmark topics — `nmbrs bench <TAB>`.
pub(crate) fn bench_topic_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    filter_prefix(&["wiring"], partial)
}

/// Yaml/json document completion for `spec=` (OpenAPI source
/// files). Reuses the workload file walker — same constraints
/// (depth-capped cwd scan), no catalog names.
pub(crate) fn spec_file_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    workload_file_candidates(partial)
}

/// Handle the `completions` subcommand:
///
/// - `nmbrs completions` (no args) — print one `source <(...)`
///   activation line on **stdout** and explanatory comments on
///   **stderr**. Splitting streams matters: with comments on
///   stdout, `` eval `nmbrs completions` `` collapses newlines
///   via word-splitting and the leading `#` makes the whole
///   joined line a comment that does nothing. Stderr is
///   visible standalone but invisible to substitution, so
///   every common eval form works:
///   `eval "$(nmbrs completions)"`, `eval $(nmbrs completions)`,
///   `` eval `nmbrs completions` ``.
/// - `nmbrs completions --shell bash` — emit the raw bash shim
///   that registers the binary as the completer. This is what
///   the activation line's `source <(... --shell bash)` pulls
///   in.
pub fn print_completions(args: &[String]) {
    let shell = args
        .iter()
        .position(|a| a == "--shell")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str());
    match shell {
        Some(name) => {
            let s = match name {
                "bash" => veks_completion::Shell::Bash,
                "zsh" => veks_completion::Shell::Zsh,
                "fish" => veks_completion::Shell::Fish,
                "elvish" => veks_completion::Shell::Elvish,
                "powershell" => veks_completion::Shell::PowerShell,
                other => {
                    eprintln!(
                        "nmbrs: unknown shell '{other}' (try bash, zsh, fish, elvish, powershell)"
                    );
                    return;
                }
            };
            veks_completion::print_completions("nmbrs", s);
        }
        None => {
            print_activation_line();
        }
    }
}

/// Resolve the path the user invoked us as, so the bash shim's
/// `source <(...)` re-invocation reaches the same binary.
fn current_exe_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok())
        .unwrap_or_else(|| "nmbrs".to_string())
}

fn print_activation_line() {
    let exe = current_exe_path();
    eprintln!("# nmbrs tab-completion for bash");
    eprintln!("# To activate:  eval \"$(nmbrs completions)\"");
    eprintln!("# To persist:   echo 'eval \"$(nmbrs completions)\"' >> ~/.bashrc");
    println!("source <(\"{exe}\" completions --shell bash)");
}

/// Handle the bash-side completion callback (`_NMBRS_COMPLETE=bash`).
/// Returns `true` if the env var was set and candidates were
/// emitted — the caller should exit immediately.
///
/// Wraps `veks_completion::handle_complete_env` with one
/// post-process: when the cursor sits on a flag that requires
/// a value (e.g. `--name`, `--metric`, …), advance past it and
/// run completion on the value position. This means
/// `nmbrs plot ... --name<TAB>` produces the available plot
/// names instead of just echoing back `--name` — there's only
/// one possible continuation (a value), so we may as well
/// take it.
pub fn handle_complete_env(tree: &CommandTree) -> bool {
    let env_set = std::env::var("_NMBRS_COMPLETE").ok().as_deref() == Some("bash")
        || std::env::var("COMPLETE").ok().as_deref() == Some("bash");
    if !env_set {
        return false;
    }

    let argv: Vec<String> = std::env::args().collect();
    let line = argv.get(1).cloned().unwrap_or_default();
    let point: usize = argv
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(line.len());
    let (prior, cur) = split_line_local(&line, point);

    // Specialised dispatch: `nmbrs metrics match <pattern>`
    // taps into the dimensional-label cache rather than
    // veks's flag-list. Caught before the flag-takes-value
    // pre-process so the partial pattern isn't auto-advanced.
    if matches_metrics_match(&prior) {
        let db_path = match db_path_from_args(&prior) {
            Some(p) => p,
            None => nmbrs_runtime::session::latest_metrics_db(),
        };
        if db_path.exists() {
            for c in crate::metrics_cache::match_completions(&cur, &db_path) {
                println!("{c}");
            }
        }
        return true;
    }

    let (eff_prior, eff_cur) = if flag_takes_value(&cur) {
        let mut p = prior.clone();
        p.push(cur.clone());
        (p, String::new())
    } else if let Some((flag, partial)) = split_attached_value(&cur) {
        // `--flag=<partial>`: complete the VALUE. Readline's word-splitting breaks
        // on `=`, so the candidates are bare values — the same shape the `key=`
        // params already emit.
        let mut p = prior.clone();
        p.push(flag);
        (p, partial)
    } else {
        (prior, cur)
    };

    let mut words_owned: Vec<String> = vec!["nmbrs".to_string()];
    words_owned.extend(eff_prior);
    words_owned.push(eff_cur);
    let words: Vec<&str> = words_owned.iter().map(String::as_str).collect();

    // Tap-tier rotation (veks 1.3.1): `complete()` is pinned to
    // tier 1, so successive tabs would never reveal Secondary /
    // FullSurface commands. Detect the rapid-tap count (same
    // cadence rule + file shape as veks's own env handler) and
    // route through the rotating completer.
    let tap = detect_tap("nmbrs", &line, tree.max_level());
    for c in veks_completion::complete_rotating_with_raw(tree, &words, tap, &line, point) {
        println!("{c}");
    }
    true
}

/// True when the prior tokens land at the positional pattern
/// of `nmbrs metrics match`. Honours intervening `--db` /
/// `--session` flag pairs that the user might type before the
/// pattern; the test is "the last two non-flag-pair tokens
/// are `metrics` and `match`".
fn matches_metrics_match(prior: &[String]) -> bool {
    let tokens: Vec<&String> = strip_flag_value_pairs(prior);
    let n = tokens.len();
    n >= 2 && tokens[n - 2] == "metrics" && tokens[n - 1] == "match"
}

/// Remove flag/value pairs (e.g. `--db PATH`, `--session NAME`)
/// from a token list so positional-relative checks see only
/// the bare positional words.
fn strip_flag_value_pairs(tokens: &[String]) -> Vec<&String> {
    let mut out: Vec<&String> = Vec::new();
    let mut iter = tokens.iter().peekable();
    while let Some(t) = iter.next() {
        if flag_takes_value(t) {
            // Skip the value too — assumed to be the next
            // token (space-form). `=`-form flags are single
            // tokens and are dropped here as well.
            let _ = iter.next();
            continue;
        }
        if t.starts_with("--") {
            // Bare flag (no value) — drop and continue.
            continue;
        }
        out.push(t);
    }
    out
}

/// Pull `--db <path>` (space- or `=`-form) out of an arg list
/// so the metrics-match completer can target the right db.
/// Falls back to None — caller defaults to `logs/latest`.
fn db_path_from_args(args: &[String]) -> Option<std::path::PathBuf> {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--db" {
            return iter.next().map(std::path::PathBuf::from);
        }
        if let Some(v) = a.strip_prefix("--db=") {
            return Some(std::path::PathBuf::from(v));
        }
    }
    None
}

/// Tokenize a shell line up to `point`, mirroring veks's
/// internal `split_line`: honors quotes + escapes, preserves
/// `=` as part of a token, drops the binary name.
fn split_line_local(line: &str, point: usize) -> (Vec<String>, String) {
    let point = point.min(line.len());
    let head = &line[..point];
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quote: Option<char> = None;
    let mut chars = head.chars().peekable();
    while let Some(ch) = chars.next() {
        match in_quote {
            Some(q) if ch == q => {
                in_quote = None;
            }
            Some(_) => cur.push(ch),
            None => match ch {
                '\'' | '"' => {
                    in_quote = Some(ch);
                }
                '\\' => {
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                }
                ' ' | '\t' => {
                    if !cur.is_empty() {
                        words.push(std::mem::take(&mut cur));
                    }
                }
                _ => cur.push(ch),
            },
        }
    }
    if !words.is_empty() {
        words.remove(0);
    }
    (words, cur)
}

/// File-backed rapid-tap counter, mirroring veks-completion's
/// internal driver (which is private): previous [`TapState`] is
/// read from `<tmp>/.nmbrs_tap_<shell-pid>`, the pure
/// [`veks_completion::next_tap_state`] cadence rule advances it,
/// and the new state is written back. Keyed by the invoking
/// shell's PID so concurrent shells rotate independently.
fn detect_tap(app: &str, input_key: &str, max_level: u32) -> u32 {
    use std::io::Write;
    // Keyed by the invoking shell's PID. std only exposes
    // parent_id() on Unix; on Windows fall back to a single
    // shared key — concurrent shells then share one tap cadence
    // file, a cosmetic degradation for a completion nicety.
    #[cfg(unix)]
    let ppid = std::os::unix::process::parent_id();
    #[cfg(not(unix))]
    let ppid = 0u32;
    let tap_file = std::env::temp_dir().join(format!(".{app}_tap_{ppid}"));
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let cur_key = input_key.trim_end();

    let prev_owned: Option<(veks_completion::TapState, String)> =
        std::fs::read_to_string(&tap_file).ok().map(|content| {
            let mut parts = content.splitn(3, ' ');
            let time_ms: u128 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let count: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let key = parts.next().unwrap_or("").trim_end().to_string();
            (veks_completion::TapState { time_ms, count }, key)
        });
    let prev = prev_owned.as_ref().map(|(st, k)| (*st, k.as_str()));

    let (tap_count, next) = veks_completion::next_tap_state(prev, now_ms, cur_key, max_level);
    if let Ok(mut f) = std::fs::File::create(&tap_file) {
        let _ = write!(f, "{} {} {}", next.time_ms, next.count, cur_key);
    }
    tap_count
}

/// Orthogonal dispatch flags on the report/plot/table commands — the ones the
/// builder recognises that are not vocab-driven. Shared by
/// [`kind_subcommand_node`] (which advertises them) and
/// [`value_taking_flags`] (which treats them as value-taking), so the two cannot
/// disagree about what the surface is.
/// Boolean flags the plot/table RENDERERS consume that live in no spec walk
/// (`summary::parse_args` / `plot_metrics::parse_args`). Unioned into
/// [`known_flags`] so the report dispatcher's closed-surface check accepts
/// what the renderers implement.
/// kv forms every `nmbrs report` path consumes (`extract_workload` plucks
/// `workload=` anywhere in the args; `session=` resolves via
/// `read_session_dir`). Declared on the report group AND its verb
/// subleafs so `nmbrs report [--synthesized] workload=<TAB>` completes
/// workload references the same way `nmbrs run workload=<TAB>` does.
// ─── Canonical parameter definitions (define once, reference everywhere) ──
//
// A parameter that appears on more than one command is defined HERE —
// key/flag spelling AND its dynamic lookup — and every declaring site
// references the definition. No site re-states a provider, so a new
// command accepting the parameter gets identical completion by
// construction.

/// `workload=<ref>` — dynamic SRD-85 lookup (local files + catalog).
pub(crate) const WORKLOAD_KV_PARAM: crate::cli_spec::KvParam = crate::cli_spec::KvParam {
    key: "workload=",
    provider: workload_provider,
};

/// `session=<name>` — completes session names.
pub(crate) const SESSION_KV_PARAM: crate::cli_spec::KvParam = crate::cli_spec::KvParam {
    key: "session=",
    provider: session_name_provider,
};

/// The workload reference as a flag/positional VALUE.
pub(crate) const WORKLOAD_VALUE: crate::cli_spec::ValueProvider =
    crate::cli_spec::ValueProvider::Custom(workload_provider);

/// `--workload <ref>` — the flag spelling. Help is per-site; lookup is not.
pub(crate) fn workload_flag(help: &'static str) -> crate::cli_spec::Flag {
    crate::cli_spec::Flag {
        long: "--workload",
        short: None,
        aliases: &[],
        arity: crate::cli_spec::Arity::Value,
        value: WORKLOAD_VALUE,
        help,
        repeatable: false,
    }
}

/// `--db <path>` — metrics-db override, shared by every read-side command.
pub(crate) fn db_flag() -> crate::cli_spec::Flag {
    crate::cli_spec::Flag {
        long: "--db",
        short: None,
        aliases: &[],
        arity: crate::cli_spec::Arity::Value,
        value: crate::cli_spec::ValueProvider::Path,
        help: "Override metrics db. Default: sessions/latest/metrics.db",
        repeatable: false,
    }
}

/// A session name/path as a flag or positional VALUE.
pub(crate) const SESSION_NAME_VALUE: crate::cli_spec::ValueProvider =
    crate::cli_spec::ValueProvider::Custom(session_name_provider);

/// An execution id as a flag VALUE.
pub(crate) const EXECUTION_ID_VALUE: crate::cli_spec::ValueProvider =
    crate::cli_spec::ValueProvider::Custom(execution_id_provider);

/// `--execution <id>` / `--all-executions` — the SRD-77 execution
/// qualifier pair, shared by every command that reads a multi-execution
/// session db (metrics subcommands, replay).
pub(crate) fn execution_flags() -> Vec<crate::cli_spec::Flag> {
    use crate::cli_spec::{Arity, Flag, ValueProvider};
    vec![
        Flag {
            long: "--execution",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: EXECUTION_ID_VALUE,
            help: "Filter to one execution_id (default: most recent).",
            repeatable: false,
        },
        Flag {
            long: "--all-executions",
            short: None,
            aliases: &[],
            arity: Arity::Bool,
            value: ValueProvider::None,
            help: "Pull every execution's instances (aggregate read).",
            repeatable: false,
        },
    ]
}

/// Free-form shared kv keys (no enumerable values, one definition each).
macro_rules! ff_kv {
    ($name:ident, $key:literal) => {
        pub(crate) const $name: crate::cli_spec::KvParam = crate::cli_spec::KvParam {
            key: $key,
            provider: free_form,
        };
    };
}
ff_kv!(CYCLES_KV_PARAM, "cycles=");
ff_kv!(THREADS_KV_PARAM, "threads=");
ff_kv!(RATE_KV_PARAM, "rate=");
ff_kv!(ERRORS_KV_PARAM, "errors=");
ff_kv!(FILENAME_KV_PARAM, "filename=");
ff_kv!(OUTPUT_KV_PARAM, "output=");
ff_kv!(STANZA_CONCURRENCY_KV_PARAM, "stanza_concurrency=");
// Seconds. `sysmon=` names the categories; this sets the sample
// window. The runtime has always read it (`runner.rs`, and it is
// documented on the sysmon module and in its error text), but it
// was never declared here — so the spec-derived argv validation
// hard-rejected a parameter the tool advertises. Free-form: a
// duration in seconds has no closed set to offer.
ff_kv!(SYSMON_INTERVAL_KV_PARAM, "sysmon-interval=");

/// `adapter=` / `driver=` — adapter inventory lookup.
pub(crate) const ADAPTER_KV_PARAM: crate::cli_spec::KvParam = crate::cli_spec::KvParam {
    key: "adapter=",
    provider: adapter_provider,
};
pub(crate) const DRIVER_KV_PARAM: crate::cli_spec::KvParam = crate::cli_spec::KvParam {
    key: "driver=",
    provider: adapter_provider,
};

/// `seq=` — the SRD sequencer type, one closed set everywhere.
pub(crate) const SEQ_KV_PARAM: crate::cli_spec::KvParam = crate::cli_spec::KvParam {
    key: "seq=",
    provider: static_seq,
};

/// The lone `workload=` kv, for commands whose runtime plucks it but
/// whose surface has no other kv (check).
pub(crate) static WORKLOAD_KV: &[crate::cli_spec::KvParam] = &[WORKLOAD_KV_PARAM];

pub(crate) static REPORT_KV: &[crate::cli_spec::KvParam] = &[WORKLOAD_KV_PARAM, SESSION_KV_PARAM];

pub(crate) const RENDERER_BOOL_FLAGS: &[&str] = &["--no-report", "--verbose", "--create", "--tree"];

pub(crate) const REPORT_DISPATCH_VALUE_FLAGS: &[&str] = &[
    "--name",
    "--at",
    "--contextual",
    "--rename",
    "--group",
    "--workload",
    "--session",
    "--db",
    "--body",
    "--body-file",
];

/// Every flag spelling declared as taking a value, DERIVED from the command spec
/// plus the report vocab.
///
/// This was a hand-maintained `matches!` list — the second list the `cli_spec`
/// architecture exists to eliminate. It had drifted: `--execution` and `--range`
/// (declared `Arity::Value` in `metrics_cmd`) were absent, and any flag added
/// since was too. A flag missing here does not auto-advance to value position and
/// its `--flag=<partial>` form completes nothing, so a correctly declared flag
/// still behaves as though it takes no value.
fn value_taking_flags() -> &'static std::collections::HashSet<String> {
    static SET: std::sync::OnceLock<std::collections::HashSet<String>> = std::sync::OnceLock::new();
    SET.get_or_init(|| {
        let mut out: std::collections::HashSet<String> = std::collections::HashSet::new();

        fn walk(cmd: &crate::cli_spec::Command, out: &mut std::collections::HashSet<String>) {
            for f in &cmd.flags {
                if matches!(f.arity, crate::cli_spec::Arity::Value) {
                    out.insert(f.long.to_string());
                    if let Some(short) = f.short {
                        out.insert(short.to_string());
                    }
                    for a in f.aliases {
                        out.insert(a.to_string());
                    }
                }
            }
            for sub in &cmd.subcommands {
                walk(sub, out);
            }
        }
        walk(&crate::cli_spec::root::root(), &mut out);

        // Report/plot/table declare no `flags` of their own — their surface comes
        // from the vocab registry through `completion_override`, so a spec walk
        // cannot see it.
        for kind in [
            nmbrs_workload::report::Kind::Plot,
            nmbrs_workload::report::Kind::Table,
            nmbrs_workload::report::Kind::Text,
            nmbrs_workload::report::Kind::File,
            nmbrs_workload::report::Kind::Details,
        ] {
            for f in nmbrs_workload::report::vocab::cli_flags_for(kind) {
                out.insert(f.to_string());
            }
        }
        for f in REPORT_DISPATCH_VALUE_FLAGS {
            out.insert((*f).to_string());
        }
        // The plot/report parser keeps its OWN list of value-taking flags, which it
        // reads to tell a flag's value from a positional spec. That list is
        // authoritative for those commands (`--filter`, the `--y*` axis family,
        // …), so union it rather than restate it here.
        for f in crate::plot_metrics::FLAGS_TAKING_VALUE {
            out.insert((*f).to_string());
        }
        out
    })
}

/// Flags whose grammar requires a value (the `--flag value` /
/// `--flag=value` shape). When the cursor sits on one of these
/// with no trailing whitespace, completion auto-advances to
/// value-position so the user gets one tab instead of two.
fn flag_takes_value(cur: &str) -> bool {
    value_taking_flags().contains(cur)
}

/// Every `--flag` spelling the CLI accepts ANYWHERE — the spec walk
/// (both arities, shorts and aliases included) unioned with the report
/// vocab and the plot parser's own list, derived exactly as
/// [`value_taking_flags`] is so there is no hand-synced copy to drift.
///
/// This is the closed surface `report_command`'s kindless dispatch
/// checks before classifying a leading `--token` as renderer
/// flag-form: a token in NO vocabulary is a typo to reject by name
/// (with prefix suggestions drawn from this same set), not a
/// selection to silently forward into an unrelated complaint.
pub(crate) fn known_flags() -> &'static std::collections::HashSet<String> {
    static SET: std::sync::OnceLock<std::collections::HashSet<String>> = std::sync::OnceLock::new();
    SET.get_or_init(|| {
        let mut out: std::collections::HashSet<String> = std::collections::HashSet::new();

        fn walk(cmd: &crate::cli_spec::Command, out: &mut std::collections::HashSet<String>) {
            for f in &cmd.flags {
                out.insert(f.long.to_string());
                if let Some(short) = f.short {
                    out.insert(short.to_string());
                }
                for a in f.aliases {
                    out.insert(a.to_string());
                }
            }
            for sub in &cmd.subcommands {
                walk(sub, out);
            }
        }
        walk(&crate::cli_spec::root::root(), &mut out);

        for kind in [
            nmbrs_workload::report::Kind::Plot,
            nmbrs_workload::report::Kind::Table,
            nmbrs_workload::report::Kind::Text,
            nmbrs_workload::report::Kind::File,
            nmbrs_workload::report::Kind::Details,
        ] {
            for f in nmbrs_workload::report::vocab::cli_flags_for(kind) {
                out.insert(f.to_string());
            }
        }
        for f in REPORT_DISPATCH_VALUE_FLAGS {
            out.insert((*f).to_string());
        }
        for f in RENDERER_BOOL_FLAGS {
            out.insert((*f).to_string());
        }
        for f in crate::plot_metrics::FLAGS_TAKING_VALUE {
            out.insert((*f).to_string());
        }
        out
    })
}

/// Split an attached `--flag=<partial>` into its flag and partial value, when the
/// flag is one that takes a value.
///
/// Without this, `--execution=<TAB>` completed NOTHING while `--execution <TAB>`
/// worked: the pre-process below only recognised a cursor sitting exactly on the
/// flag, so the attached spelling never reached the value provider. Both spellings
/// are accepted by the parsers, so both must complete.
fn split_attached_value(cur: &str) -> Option<(String, String)> {
    if !cur.starts_with('-') {
        return None;
    }
    let (flag, partial) = cur.split_once('=')?;
    if !flag_takes_value(flag) {
        return None;
    }
    Some((flag.to_string(), partial.to_string()))
}

// ---------------------------------------------------------------------------
// Per-command nodes
// ---------------------------------------------------------------------------

/// Build the per-kind subcommand node from the SRD-64 vocab
/// registry. Every flag applicable to `kind` is exposed; closed-
/// set value providers attach for directives whose vocab entry
/// declares one. Db-derived providers (metric / label-key /
/// label-value-pair) re-use the existing
/// [`metric_provider`] / [`series_provider`] / [`filter_provider`]
/// plumbing.
/// First-positional words `report_command` accepts besides an item name, so tab
/// offers the whole vocabulary of that slot rather than only half of it.
const REPORT_POSITIONAL_WORDS: &[&str] = &[
    "all", "list", "show", "figure", "rename", "scratch", "synth",
];

fn with_report_words(mut names: Vec<String>, partial: &str) -> Vec<String> {
    names.extend(
        REPORT_POSITIONAL_WORDS
            .iter()
            .filter(|w| w.starts_with(partial))
            .map(|w| (*w).to_string()),
    );
    names.sort();
    names.dedup();
    names
}

/// How many positional slots the item-name provider serves.
///
/// veks counts every non-`-` word as a positional, and `workload=<file>` /
/// `session=<dir>` are exactly that shape — so with the default single slot, one
/// `workload=` token exhausted the budget and the name stopped completing:
/// `nmbrs table comp` offered `compaction_shape` while
/// `nmbrs table workload=x.yaml comp` offered nothing, though the second is the
/// form the tool's own hint text tells you to use. Serving several slots lets the
/// provider decide for itself, which is what [`report_name_slot_open`] does.
const REPORT_POSITIONAL_SLOTS: usize = 4;

/// Whether the item-name slot is still unfilled.
///
/// Everything before the name is a `key=value` param, a flag, or a flag's value.
/// A bare word that is none of those IS the name, and once one is present the
/// provider must go quiet rather than suggest a second.
fn report_name_slot_open(ctx: &[&str]) -> bool {
    let mut i = 0;
    while i < ctx.len() {
        let w = ctx[i];
        if w.starts_with('-') {
            // Skip a value-taking flag's value; `--flag=value` is self-contained.
            if !w.contains('=') && flag_takes_value(w) {
                i += 1;
            }
        } else if !w.contains('=') && !REPORT_POSITIONAL_WORDS.contains(&w) {
            return false; // a bare, non-keyword word — that's the name
        }
        i += 1;
    }
    true
}

/// Positional completion for `nmbrs plot <name>` / `nmbrs report plot <name>`.
///
/// `fn_provider` takes a plain fn pointer, so each kind gets its own thin
/// wrapper rather than a closure over the kind.
fn plot_positional_provider(partial: &str, ctx: &[&str]) -> Vec<String> {
    if !report_name_slot_open(ctx) {
        return Vec::new();
    }
    with_report_words(plot_name_provider(partial, ctx), partial)
}

/// Positional completion for `nmbrs table <name>` / `nmbrs report table <name>`.
fn table_positional_provider(partial: &str, ctx: &[&str]) -> Vec<String> {
    if !report_name_slot_open(ctx) {
        return Vec::new();
    }
    with_report_words(summary_name_provider(partial, ctx), partial)
}

pub(crate) fn kind_subcommand_node(kind: nmbrs_workload::report::Kind) -> Node {
    use nmbrs_workload::report::vocab::{self, ValueProvider};

    let flags: Vec<&'static str> = vocab::cli_flags_for(kind);
    // Orthogonal dispatch flags (not vocab-driven) — same set
    // the builder recognises in `report_build::Dispatch`.
    let mut all_value_flags: Vec<&'static str> = flags.clone();
    all_value_flags.extend(REPORT_DISPATCH_VALUE_FLAGS.iter().copied());
    // Bare `key=value` spellings the report handlers accept alongside the `--`
    // forms. Undeclared, they completed as nothing and read as an unrecognised
    // token, so `table x session=<dir>` silently reported on `sessions/latest`
    // while naming another directory.
    all_value_flags.extend(REPORT_KV.iter().map(|kv| kv.key));
    let bool_flags: &[&str] = &[
        // `--create` persists the spec under `--name`; it takes no value
        // (`opts.create = true` in summary's parser). The retired
        // `flag_takes_value` list wrongly counted it as value-taking, so
        // completion advanced past it and treated the NEXT token as its value.
        "--add",
        "--replace",
        "--stdout",
        "--ascii",
        "--dry-run",
        "--create",
    ];

    let mut node = Node::leaf_with_flags(&all_value_flags, bool_flags);

    // Per-vocab-flag value providers. `fn_provider` takes a
    // function pointer (not a closure), so we route each
    // closed set through a dedicated tiny `fn` rather than a
    // factory closure.
    for d in vocab::ALL_DIRECTIVES {
        if !d.applies_to.contains(kind) {
            continue;
        }
        match d.value {
            ValueProvider::Closed(_) => {
                if let Some(provider) = closed_set_provider_for(d.yaml_directive) {
                    node = node.with_value_provider(d.cli_flag, fn_provider(provider));
                }
            }
            ValueProvider::DbMetricNames => {
                node = node.with_value_provider(d.cli_flag, fn_provider(metric_provider));
            }
            ValueProvider::DbLabelKeys => {
                node = node.with_value_provider(d.cli_flag, fn_provider(series_provider));
            }
            ValueProvider::DbLabelKeyValuePairs => {
                node = node.with_value_provider(d.cli_flag, fn_provider(filter_provider));
            }
            // Number / HexColor / Json / Text / Path:
            // suggestions don't help (free-form). Leave the
            // flag declared so the parser accepts it; the
            // user types the value freely.
            _ => {}
        }
    }

    // Orthogonal dispatch-flag providers.
    node = node
        .with_value_provider("--name", fn_provider(report_any_name_provider))
        .with_value_provider("--at", fn_provider(at_anchor_provider))
        .with_value_provider("--contextual", fn_provider(contextual_mode_provider))
        .with_value_provider("--session", fn_provider(session_name_provider))
        .with_value_provider("--workload", fn_provider(workload_provider));
    // The bare kv spellings complete via the canonical definitions —
    // the same provider the kv surface everywhere else uses.
    for kv in REPORT_KV {
        node = node.with_value_provider(kv.key, fn_provider(kv.provider));
    }

    // The POSITIONAL — the item name. Only `--name` carried a provider, so
    // `nmbrs table <TAB>` offered nothing even though `nmbrs table --name <TAB>`
    // listed the very same items, and the positional is the spelling the docs and
    // everyday use favour (`nmbrs table compaction_shape`). Kind-specific, so
    // `table` offers table items and `plot` offers plots rather than both.
    node = match kind {
        nmbrs_workload::report::Kind::Plot => node
            .with_positional_provider(fn_provider(plot_positional_provider))
            .with_positional_slots(REPORT_POSITIONAL_SLOTS),
        nmbrs_workload::report::Kind::Table => node
            .with_positional_provider(fn_provider(table_positional_provider))
            .with_positional_slots(REPORT_POSITIONAL_SLOTS),
        // text/file/details aren't accepted at this position by the parser yet
        // (see the ignored `report_text_excludes_figure_directives` test), so
        // advertising names for them would promise a path that errors.
        _ => node,
    };

    node
}

/// Map a vocab directive's `yaml_directive` keyword to the
/// matching closed-set provider fn-pointer. `None` means the
/// directive's value space isn't a closed set (handled by
/// the calling match arm).
fn closed_set_provider_for(yaml_directive: &str) -> Option<crate::cli_spec::DynamicOptions> {
    match yaml_directive {
        "palette" => Some(palette_provider),
        "line" => Some(line_styles_provider),
        "marker" => Some(marker_shapes_provider),
        "agg" => Some(agg_fns_provider),
        "x-scale" | "y-scale" => Some(axis_scales_provider),
        _ => None,
    }
}

fn palette_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    nmbrs_workload::report::vocab::PALETTE_NAMES
        .iter()
        .filter(|s| s.starts_with(partial))
        .map(|s| s.to_string())
        .collect()
}

fn line_styles_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    nmbrs_workload::report::vocab::LINE_STYLES
        .iter()
        .filter(|s| s.starts_with(partial))
        .map(|s| s.to_string())
        .collect()
}

fn marker_shapes_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    nmbrs_workload::report::vocab::MARKER_SHAPES
        .iter()
        .filter(|s| s.starts_with(partial))
        .map(|s| s.to_string())
        .collect()
}

fn agg_fns_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    nmbrs_workload::report::vocab::AGG_FNS
        .iter()
        .filter(|s| s.starts_with(partial))
        .map(|s| s.to_string())
        .collect()
}

fn axis_scales_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    nmbrs_workload::report::vocab::AXIS_SCALES
        .iter()
        .filter(|s| s.starts_with(partial))
        .map(|s| s.to_string())
        .collect()
}

/// Closed set for `--at <scope>`: `root` plus the prefix forms
/// `scenario:`, `phase:`, `op:`. Past the prefix the value
/// space depends on the workload + active session, which is
/// out of scope for this surface — completion stops at the
/// prefix and the user types the name.
fn at_anchor_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    ["root", "scenario:", "phase:", "op:"]
        .iter()
        .filter(|s| s.starts_with(partial))
        .map(|s| s.to_string())
        .collect()
}

/// Closed set for `--contextual <mode>`.
fn contextual_mode_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    ["auto", "root", "scenario", "phase", "op"]
        .iter()
        .filter(|s| s.starts_with(partial))
        .map(|s| s.to_string())
        .collect()
}

fn table_node() -> StrictNode<true, true> {
    // Unadvertised alias for `nmbrs report table ...` (SRD-46).
    StrictNode::leaf_with_flags(&["--db", "--format", "--output", "--name"], &["--create"])
        .with_value_provider("--name", fn_provider(summary_name_provider))
        .with_value_provider("workload=", fn_provider(workload_provider))
        .with_category(Category::Tools.tag())
        .with_level(Level::Secondary.rank())
}

fn polydat_node() -> StrictNode<true, true> {
    // `nmbrs Polydat visualize <expr|file.polydat>`. Lone subcommand for
    // now; sibling slots (`polydat functions`, `polydat dag`) live under
    // `describe wiring` until a broader polydat-subcommand refactor.
    StrictNode::leaf_with_flags(&[], &[])
        .with_category(Category::Tools.tag())
        .with_level(Level::FullSurface.rank())
}

fn metrics_node() -> StrictNode<true, true> {
    // `nmbrs metrics <list|show|match> [<expr>]` — read-side
    // introspection over the active session db. Flag lists are
    // sourced from `metrics_cmd` (LIST_FLAGS / MATCH_FLAGS) so
    // the parser and completion stay in lockstep — adding a
    // flag in one place is enough to surface it in tab.
    let list_flags = crate::metrics_cmd::list_all_flags();
    let list_bools = crate::metrics_cmd::LIST_BOOL_FLAGS;
    let match_flags = crate::metrics_cmd::match_all_flags();
    StrictNode::group(vec![
        // SRD-93: `list` is the structure view (`show` its deprecated
        // alias); `summarize` carries the per-leaf value summaries.
        (
            "list",
            Node::leaf_with_flags(&list_flags, list_bools)
                .with_value_provider("--format", fn_provider(static_metrics_format)),
        ),
        (
            "summarize",
            Node::leaf_with_flags(&list_flags, list_bools)
                .with_value_provider("--format", fn_provider(static_metrics_format)),
        ),
        (
            "show",
            Node::leaf_with_flags(&list_flags, list_bools)
                .with_value_provider("--format", fn_provider(static_metrics_format)),
        ),
        ("match", Node::leaf_with_flags(&match_flags, &[])),
    ])
    .with_category(Category::Tools.tag())
    .with_level(Level::Secondary.rank())
}

/// Closed-set value provider for `nmbrs metrics list/show
/// --format`. Sourced from `metrics_cmd::FORMAT_VALUES` so
/// adding a format keyword automatically appears in tab.
fn static_metrics_format(partial: &str, _ctx: &[&str]) -> Vec<String> {
    filter_prefix(crate::metrics_cmd::FORMAT_VALUES, partial)
}

fn describe_node() -> StrictNode<true, true> {
    // Subcommand tree so TAB walks `describe → wiring →
    // functions [--verbose]` etc. Each leaf is its own node so
    // we can attach flag completions appropriate to the
    // subtopic (e.g. `--verbose` is only meaningful for
    // `wiring functions`).
    StrictNode::group(vec![
        ("adapter", Node::leaf(&[])),
        (
            "wiring",
            Node::group(vec![
                (
                    "functions",
                    Node::leaf_with_flags(&[], &["--verbose", "-v"]),
                ),
                ("functions-md", Node::leaf(&[])),
                ("types", Node::leaf(&[])),
                ("types-md", Node::leaf(&[])),
                ("stdlib", Node::leaf(&[])),
                ("dag", Node::leaf(&[])),
                ("modules", Node::leaf(&[])),
            ]),
        ),
        ("wrappers", Node::leaf(&[])),
        (
            "op",
            Node::leaf(&[]).with_value_provider("workload=", fn_provider(workload_provider)),
        ),
    ])
    .with_category(Category::Documentation.tag())
    .with_level(Level::FullSurface.rank())
}

fn bench_node() -> StrictNode<true, true> {
    StrictNode::leaf(&[])
        .with_category(Category::Benchmark.tag())
        .with_level(Level::FullSurface.rank())
}

fn plot_node() -> StrictNode<true, true> {
    StrictNode::leaf_with_flags(
        &[
            "--db",
            "--output",
            "--metric",
            "--x",
            "--series",
            "--filter",
            "--agg",
            "--name",
            "--title",
            "--xlabel",
            "--ylabel",
            "--x-scale",
            "--y-scale",
            "--width",
            "--height",
            "--scale",
            "--csv-also",
        ],
        &["--verbose"],
    )
    .with_value_provider("--name", fn_provider(plot_name_provider))
    .with_value_provider("--metric", fn_provider(metric_provider))
    .with_value_provider("--series", fn_provider(series_provider))
    .with_value_provider("--x", fn_provider(series_provider))
    .with_value_provider("--filter", fn_provider(filter_provider))
    // `workload=<file.yaml>` sources named plots from the
    // YAML's `plot:` block instead of the metrics db.
    .with_value_provider("workload=", fn_provider(workload_provider))
    .with_category(Category::Tools.tag())
    // Same tier as `summary` — both are post-hoc analysis
    // tools over the metrics db, both replay stored named
    // specs by `--name`. Surfacing them at the same TAB
    // level keeps the UX symmetrical.
    .with_level(Level::Secondary.rank())
}

fn web_node() -> StrictNode<true, true> {
    StrictNode::leaf(&[])
        .with_category(Category::Server.tag())
        .with_level(Level::FullSurface.rank())
}

fn completions_node() -> StrictNode<true, true> {
    StrictNode::leaf(&["--shell"])
        .with_category(Category::Shell.tag())
        .with_level(Level::FullSurface.rank())
}

// ---------------------------------------------------------------------------
// Value providers (hoisted from nmbrs-runtime::completions)
// ---------------------------------------------------------------------------

pub(crate) fn workload_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    // Tier 1 — the obvious candidates: local files and catalog
    // names whose full reference begins with what's typed. SRD-85:
    // bundled workloads run by catalog name from any directory, so
    // they complete alongside local files (examples are hidden from
    // the default *listing*, not from use).
    let mut out = workload_file_candidates(partial);
    out.extend(
        nmbrs_workload::catalog::iter()
            .map(|w| w.name.to_string())
            .filter(|n| n.starts_with(partial)),
    );
    out.sort();
    out.dedup();
    if !out.is_empty() {
        return out;
    }
    // Tier 2 — nothing obvious near the cursor: fall back to the
    // shared deep leaf-segment search across the local hierarchy
    // (down to 3 levels) and the bundled catalog, offering every
    // match however deep. This is what lets a bare `phase_poll`
    // surface a buried `examples/controls/phase_poll_smoke`.
    nmbrs_workload::suggest::suggest_workloads(partial)
}

fn scenario_provider(partial: &str, ctx: &[&str]) -> Vec<String> {
    let context_strings: Vec<String> = ctx.iter().map(|s| s.to_string()).collect();
    scenario_candidates(partial, &context_strings)
}

/// Dynamic completion for `--execution=<n>` (SRD-77). Reads
/// the session db at `logs/latest/metrics.db` and returns
/// every distinct `exec_id` from the `executions` table.
/// Best-effort: any sqlite error (missing file, missing
/// table, lock contention) yields an empty list rather than
/// an error message, matching the existing provider
/// convention.
pub fn execution_id_provider(partial: &str, ctx: &[&str]) -> Vec<String> {
    // Resolve the db from the command line — `--db`, the `--session*` family,
    // or a bare `session=` — instead of assuming the latest session. Hardcoding
    // `latest_metrics_db()` offered values from a DIFFERENT session than the one
    // being targeted: with `--session=<dir>` naming a session holding only
    // exec 77, completion still offered `0 1` from `sessions/latest`. Values that
    // do not exist in the named session are worse than no suggestion at all.
    let db_path = db_path_from_context(ctx);
    if !db_path.exists() {
        return Vec::new();
    }
    let conn = match rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    // Verify table exists before querying — sessions from
    // before SRD-77 won't have it.
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
         WHERE type='table' AND name='executions')",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n != 0)
        .unwrap_or(false);
    if !exists {
        return Vec::new();
    }
    let mut stmt = match conn.prepare("SELECT exec_id FROM executions ORDER BY exec_id") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map([], |r| r.get::<_, i64>(0)) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    rows.filter_map(Result::ok)
        .map(|id| id.to_string())
        .filter(|s| s.starts_with(partial))
        .collect()
}

fn adapter_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    let mut names: Vec<String> = registered_driver_names()
        .into_iter()
        .map(|s| s.to_string())
        .filter(|n| n.starts_with(partial))
        .collect();
    names.sort();
    names
}

fn static_profiler(partial: &str, _ctx: &[&str]) -> Vec<String> {
    filter_prefix(&["off", "flamegraph", "perf"], partial)
}

fn static_tui(partial: &str, _ctx: &[&str]) -> Vec<String> {
    filter_prefix(&["on", "off"], partial)
}

/// Closed-set value provider for `format=` (the stdout adapter's render
/// format). Sourced from `nmbrs_adapter_stdout::FORMAT_NAMES` so the
/// parser, its error message, and completion stay in lockstep.
fn static_stdout_format(partial: &str, _ctx: &[&str]) -> Vec<String> {
    filter_prefix(nmbrs_adapter_stdout::FORMAT_NAMES, partial)
}

fn static_sysmon(partial: &str, _ctx: &[&str]) -> Vec<String> {
    // Comma lists complete per segment: `sysmon=cpu,i<TAB>` completes `io`.
    let (done, part) = match partial.rfind(',') {
        Some(i) => (&partial[..=i], &partial[i + 1..]),
        None => ("", partial),
    };
    filter_prefix(
        &["all", "any", "cpu", "io", "ram", "rambw", "storage"],
        part,
    )
    .into_iter()
    .map(|c| format!("{done}{c}"))
    .collect()
}

fn static_dryrun(partial: &str, _ctx: &[&str]) -> Vec<String> {
    filter_prefix(
        &[
            "phase",
            "dispenser",
            "op",
            "cycle",
            "full",
            "controls",
            "wiring",
            "labels",
            "silent",
            "fields",
            "json",
        ],
        partial,
    )
}

/// Static completions for `scope=`. SRD-77 refine policy.
fn static_scope(partial: &str, _ctx: &[&str]) -> Vec<String> {
    filter_prefix(&["missing", "changed", "all"], partial)
}

/// Static completions for `on_removed=`. SRD-77 refine
/// removed-phase policy.
fn static_on_removed(partial: &str, _ctx: &[&str]) -> Vec<String> {
    filter_prefix(&["error", "keep", "drop"], partial)
}

/// Static completions for `watch=`. Only the bare-spec
/// forms (`report`, `plot`) are offered here — the
/// `report:<args>` and `plot:<name>` shapes carry
/// arbitrary user payload, so we don't try to enumerate
/// every plot name the session might own. The bare forms
/// cover the most common workflow ("re-render everything
/// after each phase") and a typing user can extend from
/// there.
fn static_watch(partial: &str, _ctx: &[&str]) -> Vec<String> {
    filter_prefix(&["report", "plot"], partial)
}

/// Inspector socket discovery — same logic as the legacy
/// `nmbrs-runtime::completions::socket_path_candidates`.
pub(crate) fn socket_path_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    let read = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !(name.starts_with("nmbrs-") && name.ends_with(".sock")) {
            continue;
        }
        let full = path.to_string_lossy().into_owned();
        if full.starts_with(partial) {
            out.push(full);
        }
    }
    out.sort();
    out
}

pub(crate) fn pid_provider(partial: &str, _ctx: &[&str]) -> Vec<String> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    let read = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    for entry in read.flatten() {
        let Some(name) = entry
            .path()
            .file_name()
            .and_then(|s| s.to_str().map(str::to_string))
        else {
            continue;
        };
        let Some(rest) = name.strip_prefix("nmbrs-") else {
            continue;
        };
        let Some(pid_str) = rest.strip_suffix(".sock") else {
            continue;
        };
        if pid_str.parse::<u32>().is_ok() && pid_str.starts_with(partial) {
            out.push(pid_str.to_string());
        }
    }
    out.sort();
    out
}

/// SRD-46 cross-kind name provider for `nmbrs report --name`:
/// emits every plot AND table item the workload defines (or
/// the session db has persisted). Kind-filtered providers stay
/// separate so `nmbrs plot` / `nmbrs table` aliases offer only
/// their own kind.
pub(crate) fn report_any_name_provider(partial: &str, ctx: &[&str]) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    if let Some(path) = workload_from_context(ctx) {
        all.extend(crate::plot_metrics::list_workload_plot_names(&path));
        all.extend(crate::summary::list_workload_summary_names(&path));
    } else {
        let db_path = db_path_from_context(ctx);
        all.extend(crate::plot_metrics::list_stored_plot_names(&db_path));
        all.extend(crate::summary::list_stored_summary_names(&db_path));
        // Db-stored items are populated by the runner only for
        // sessions produced post-SRD-46-persistence-wiring.
        // For older sessions (or any session whose runner didn't
        // persist) `session_metadata.workload` still records the
        // workload's bare name — recover the declared items
        // from there.
        if all.is_empty()
            && let Some(yaml) = workload_path_from_session_db(&db_path)
        {
            all.extend(crate::plot_metrics::list_workload_plot_names(&yaml));
            all.extend(crate::summary::list_workload_summary_names(&yaml));
        }
    }
    all.retain(|n| n.starts_with(partial));
    all.sort();
    all.dedup();
    all
}

/// Stored-summary-name completion for `nmbrs summary --name`.
/// `workload=<path>` on the line wins (sources from the
/// workload's `summary:` block). Otherwise falls back to the
/// metrics db's `session_metadata` table.
fn summary_name_provider(partial: &str, ctx: &[&str]) -> Vec<String> {
    if let Some(path) = workload_from_context(ctx) {
        return crate::summary::list_workload_summary_names(&path)
            .into_iter()
            .filter(|n| n.starts_with(partial))
            .collect();
    }
    let db_path = db_path_from_context(ctx);
    let stored: Vec<String> = crate::summary::list_stored_summary_names(&db_path);
    if !stored.is_empty() {
        return stored
            .into_iter()
            .filter(|n| n.starts_with(partial))
            .collect();
    }
    // Db has no persisted summaries — fall back to the workload
    // recorded in `session_metadata.workload`. Same shape as
    // `report_any_name_provider`.
    if let Some(yaml) = workload_path_from_session_db(&db_path) {
        return crate::summary::list_workload_summary_names(&yaml)
            .into_iter()
            .filter(|n| n.starts_with(partial))
            .collect();
    }
    Vec::new()
}

/// Metric-family completion for `nmbrs plot --metric`. Reads
/// the session db's `metric_family` table so the user gets the
/// closed vocabulary of metrics actually recorded in this
/// session (recall_at_10_mean, cycles_total, errors_total, …).
///
/// Honours `--db`, `--session-path`, and `--session` on the
/// line so the suggestions match wherever the eventual command
/// will read.
fn metric_provider(partial: &str, ctx: &[&str]) -> Vec<String> {
    let db_path = db_path_from_context(ctx);
    crate::plot_metrics::list_metric_families(&db_path)
        .into_iter()
        .filter(|n| n.starts_with(partial))
        .collect()
}

/// Stored-plot-name completion for `nmbrs plot --name`. Same
/// rules as `summary_name_provider`: `workload=<path>` overrides
/// db lookup.
fn plot_name_provider(partial: &str, ctx: &[&str]) -> Vec<String> {
    if let Some(path) = workload_from_context(ctx) {
        return crate::plot_metrics::list_workload_plot_names(&path)
            .into_iter()
            .filter(|n| n.starts_with(partial))
            .collect();
    }
    let db_path = db_path_from_context(ctx);
    let stored: Vec<String> = crate::plot_metrics::list_stored_plot_names(&db_path);
    if !stored.is_empty() {
        return stored
            .into_iter()
            .filter(|n| n.starts_with(partial))
            .collect();
    }
    // Fallback: when the session db doesn't carry persisted
    // plot specs (older runs, or a session that finished before
    // SRD-46 plot persistence wired up), look up the workload
    // YAML from `session_metadata.workload` and read its
    // `report:` block directly.
    if let Some(yaml) = workload_path_from_session_db(&db_path) {
        return crate::plot_metrics::list_workload_plot_names(&yaml)
            .into_iter()
            .filter(|n| n.starts_with(partial))
            .collect();
    }
    Vec::new()
}

/// Label-key completion for `nmbrs plot --series` and `--x`.
///
/// Surfaces every distinct label key recorded in the metrics
/// db, narrowed to the metric family in scope when one can be
/// determined: `--metric <X>` wins, else `--name <X>` resolves
/// through the workload's `plot:` block or the db's stored
/// plots. Keys already present in `--x` / earlier `--series`
/// args are filtered out so suggestions move forward.
///
/// `--series` is comma-separated, so when the partial token
/// contains commas, the prefix part is preserved verbatim and
/// matching candidates are appended after the last comma.
fn series_provider(partial: &str, ctx: &[&str]) -> Vec<String> {
    let db_path = db_path_from_context(ctx);
    let workload_path = workload_from_context(ctx);
    let metric_pattern = metric_from_context(ctx, &db_path, workload_path.as_deref());

    let mut keys = crate::plot_metrics::list_label_keys(&db_path, metric_pattern.as_deref());

    let mut used: std::collections::HashSet<String> = used_label_keys(ctx)
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    let (head, tail) = match partial.rfind(',') {
        Some(i) => (&partial[..=i], &partial[i + 1..]),
        None => ("", partial),
    };
    for k in head.split(',') {
        let k = k.trim();
        if !k.is_empty() {
            used.insert(k.to_string());
        }
    }
    keys.retain(|k| !used.contains(k));
    keys.into_iter()
        .filter(|k| k.starts_with(tail))
        .map(|k| format!("{head}{k}"))
        .collect()
}

/// `--filter <key>=<value>`: when the partial has no `=`, suggest
/// label keys followed by `=`. Once `=` is typed we let the user
/// supply the value freely (no enumeration — the value space is
/// arbitrary strings).
fn filter_provider(partial: &str, ctx: &[&str]) -> Vec<String> {
    if partial.contains('=') {
        return Vec::new();
    }
    let db_path = db_path_from_context(ctx);
    let workload_path = workload_from_context(ctx);
    let metric_pattern = metric_from_context(ctx, &db_path, workload_path.as_deref());
    crate::plot_metrics::list_label_keys(&db_path, metric_pattern.as_deref())
        .into_iter()
        .filter(|k| k.starts_with(partial))
        .map(|k| format!("{k}="))
        .collect()
}

/// Find the metric family the user is plotting from `--metric`
/// or, failing that, the metric encoded in the named plot
/// referenced by `--name`.
fn metric_from_context(
    ctx: &[&str],
    db_path: &std::path::Path,
    workload_path: Option<&std::path::Path>,
) -> Option<String> {
    let mut iter = ctx.iter();
    while let Some(&w) = iter.next() {
        if w == "--metric"
            && let Some(&v) = iter.next()
        {
            return Some(v.to_string());
        }
        if let Some(v) = w.strip_prefix("--metric=") {
            return Some(v.to_string());
        }
    }
    let mut iter = ctx.iter();
    while let Some(&w) = iter.next() {
        if w == "--name"
            && let Some(&v) = iter.next()
        {
            return crate::plot_metrics::metric_for_plot_name(db_path, workload_path, v);
        }
        if let Some(v) = w.strip_prefix("--name=") {
            return crate::plot_metrics::metric_for_plot_name(db_path, workload_path, v);
        }
    }
    None
}

/// Collect label keys already pinned by `--x` and any prior
/// `--series` value(s) on the line. Comma-split because
/// `--series` accepts comma-separated lists.
fn used_label_keys<'a>(ctx: &'a [&'a str]) -> std::collections::HashSet<&'a str> {
    let mut out: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut iter = ctx.iter();
    while let Some(&w) = iter.next() {
        let val = if w == "--x" || w == "--series" {
            iter.next().copied()
        } else if let Some(v) = w.strip_prefix("--x=") {
            Some(v)
        } else {
            w.strip_prefix("--series=")
        };
        if let Some(v) = val {
            for k in v.split(',') {
                let k = k.trim();
                if !k.is_empty() {
                    out.insert(k);
                }
            }
        }
    }
    out
}

/// Pull `workload=<path>` from the in-progress command line, if
/// present. Resolves through the standard workload-path search
/// (`./<name>`, `<name>.yaml`, `workloads/<name>`, …) so the
/// completion provider sees the same file the command would.
fn workload_from_context(ctx: &[&str]) -> Option<std::path::PathBuf> {
    for word in ctx {
        if let Some(v) = word.strip_prefix("workload=") {
            // Three resolution shapes:
            //
            //   1. Direct path/name → `resolve_workload_path` →
            //      yaml file.
            //   2. Path to a session directory (`local/foo/`) →
            //      read `session_metadata.workload` from its
            //      `metrics.db` and resolve THAT name.
            //   3. Path to a metrics.db itself → same lookup.
            //
            // Shape (2)/(3) lets `workload=<session>` flow back
            // to the original yaml so completion / `nmbrs report`
            // can find the declared plot/table names without
            // requiring the user to know where the yaml lives.
            if let Some(p) = crate::cli::resolve_workload_path(v) {
                let pb = std::path::PathBuf::from(p);
                if pb.exists() {
                    return Some(pb);
                }
            }
            let candidate = std::path::PathBuf::from(v);
            if candidate.exists() {
                if candidate.is_file() {
                    return Some(candidate);
                }
                // Directory: try `<dir>/metrics.db`.
                let db = candidate.join("metrics.db");
                if db.exists()
                    && let Some(name) = workload_name_from_db(&db)
                    && let Some(yaml) = crate::cli::resolve_workload_path(&name)
                {
                    let p = std::path::PathBuf::from(yaml);
                    if p.exists() {
                        return Some(p);
                    }
                }
            } else if candidate.extension().is_none()
                && let Some(name) = workload_name_from_db(&candidate)
                && let Some(yaml) = crate::cli::resolve_workload_path(&name)
            {
                // Bare `workload=metrics.db`-style — try as-is.
                let p = std::path::PathBuf::from(yaml);
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// Read `session_metadata.workload` from a session db. The
/// runner records the bare workload name (no extension, no
/// path) so completion can map back to the declared yaml via
/// `resolve_workload_path`.
fn workload_name_from_db(db_path: &std::path::Path) -> Option<String> {
    if !db_path.exists() {
        return None;
    }
    let conn = rusqlite::Connection::open(db_path).ok()?;
    // `workload` is per-execution metadata; latest execution wins
    // (falls back to legacy session_metadata).
    nmbrs_metrics::reporters::sqlite::latest_execution_metadata_value(&conn, "workload")
}

/// Combine `workload_name_from_db` with `resolve_workload_path`
/// so a session db's recorded workload name flows back to the
/// declared yaml file. Returns `None` when either step fails
/// (e.g. db without metadata, or workload yaml has been moved
/// since the session ran).
fn workload_path_from_session_db(db_path: &std::path::Path) -> Option<std::path::PathBuf> {
    let name = workload_name_from_db(db_path)?;
    let yaml = crate::cli::resolve_workload_path(&name)?;
    let p = std::path::PathBuf::from(yaml);
    if p.exists() { Some(p) } else { None }
}

fn db_path_from_context(ctx: &[&str]) -> std::path::PathBuf {
    // `--db <path>` is the most explicit form — wins over any
    // session resolution.
    let mut iter = ctx.iter();
    while let Some(&w) = iter.next() {
        if w == "--db"
            && let Some(&v) = iter.next()
        {
            return std::path::PathBuf::from(v);
        }
        if let Some(v) = w.strip_prefix("--db=") {
            return std::path::PathBuf::from(v);
        }
    }
    // `--session` / `--session-path` / `--session-name` go
    // through the shared resolver so completion sees the same
    // db path the command itself will read. Single source of
    // truth for "what does --session mean".
    let owned: Vec<String> = ctx.iter().map(|s| s.to_string()).collect();
    if let Some(dir) = nmbrs_runtime::session::read_session_dir(&owned) {
        return dir.join("metrics.db");
    }
    nmbrs_runtime::session::latest_metrics_db()
}

/// Dynamic option discovery: when a `workload=…` is on the
/// line, parse the workload file and surface its declared
/// `params:` keys as completion targets.
pub(crate) fn workload_dynamic_params(_partial: &str, ctx: &[&str]) -> Vec<String> {
    let mut workload_path: Option<String> = None;
    for word in ctx {
        if let Some(p) = word.strip_prefix("workload=") {
            workload_path = Some(p.to_string());
            break;
        }
        if word.ends_with(".yaml") || word.ends_with(".yml") {
            workload_path = Some((*word).to_string());
            break;
        }
    }
    let Some(name) = workload_path else {
        return Vec::new();
    };
    let Some(yaml) = workload_text_for_completion(&name) else {
        return Vec::new();
    };
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(&yaml) else {
        return Vec::new();
    };
    let Some(params) = doc.get("params").and_then(|v| v.as_mapping()) else {
        return Vec::new();
    };
    params
        .keys()
        .filter_map(|k| k.as_str().map(|s| format!("{s}=")))
        .collect()
}

// ---------------------------------------------------------------------------
// Workload-file / scenario name discovery (hoisted from
// nmbrs-runtime::completions)
// ---------------------------------------------------------------------------

/// Maximum directory depth the walker descends from each seed
/// root. Caps the cost of `workload=<TAB>` in deep trees.
const WORKLOAD_MAX_DEPTH: usize = 4;

/// Maximum number of directory entries the walker visits in
/// one completion call. Bounds the cost of a `<TAB>` press in
/// a large tree.
const WORKLOAD_MAX_FILES_SCANNED: usize = 1000;

/// Discover workload candidates for `workload=` tab-completion.
///
/// Recursively scans up to [`WORKLOAD_MAX_DEPTH`] levels deep
/// (or [`WORKLOAD_MAX_FILES_SCANNED`] entries, whichever comes
/// first) under each seed root, emitting every yaml file as a
/// full relative path so nested workloads surface without the
/// user having to tab through each level manually.
fn workload_file_candidates(cur: &str) -> Vec<String> {
    use std::path::Path;
    let mut out: Vec<String> = Vec::new();
    let mut budget = WORKLOAD_MAX_FILES_SCANNED;
    if cur.contains('/') {
        let split = cur.rfind('/').unwrap();
        let dir_prefix = &cur[..=split];
        let name_prefix = &cur[split + 1..];
        let seed = Path::new(dir_prefix.trim_end_matches('/'));
        collect_yaml_recursive(seed, dir_prefix, name_prefix, &mut out, &mut budget, 0);
    } else {
        let roots: &[(&str, &str)] = &[
            (".", ""),
            ("workloads", "workloads/"),
            ("examples", "examples/"),
        ];
        for (dir, prefix) in roots {
            collect_yaml_recursive(Path::new(dir), prefix, cur, &mut out, &mut budget, 0);
            if budget == 0 {
                break;
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Recursive walker. Emits every yaml file at depth ≤
/// [`WORKLOAD_MAX_DEPTH`] under `dir`. At depth 0 the leaf
/// filename is filtered by `name_prefix` (the user-typed
/// partial); deeper levels descend regardless so subdir-buried
/// workloads still surface by full relative path.
fn collect_yaml_recursive(
    dir: &std::path::Path,
    emit_prefix: &str,
    name_prefix: &str,
    out: &mut Vec<String>,
    budget: &mut usize,
    current_depth: usize,
) {
    if *budget == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let Some(name_os) = entry.path().file_name().map(|n| n.to_owned()) else {
            continue;
        };
        let name = name_os.to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if current_depth == 0 && !name.starts_with(name_prefix) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            if matches!(name.as_str(), "target" | "node_modules" | "logs") {
                continue;
            }
            // Descend so files at depth N (N = WORKLOAD_MAX_DEPTH)
            // remain visible. The cap counts the deepest dir
            // entries we read, not the dir we recurse into.
            if current_depth < WORKLOAD_MAX_DEPTH {
                let child_prefix = format!("{emit_prefix}{name}/");
                collect_yaml_recursive(&path, &child_prefix, "", out, budget, current_depth + 1);
            }
            continue;
        }
        if let Some(ext) = path.extension()
            && (ext == "yaml" || ext == "yml")
        {
            out.push(format!("{emit_prefix}{name}"));
        }
    }
}

/// SRD-85: workload source text for completion — local file
/// first, bundled catalog second (ambiguity doesn't matter for
/// suggestions; local wins).
fn workload_text_for_completion(name: &str) -> Option<String> {
    if let Some(path) = resolve_workload_file_public(name) {
        return std::fs::read_to_string(path).ok();
    }
    nmbrs_workload::catalog::lookup(name).map(|w| w.source.to_string())
}

fn scenario_candidates(cur: &str, prior: &[String]) -> Vec<String> {
    let workload = prior.iter().find_map(|w| {
        if let Some(v) = w.strip_prefix("workload=") {
            Some(v.to_string())
        } else if w.ends_with(".yaml") || w.ends_with(".yml") {
            Some(w.clone())
        } else {
            None
        }
    });
    let Some(name) = workload else {
        return Vec::new();
    };
    let mut scenarios = if let Some(path) = resolve_workload_file_public(&name) {
        scenarios_in_workload_file(&path)
    } else if let Some(text) = workload_text_for_completion(&name) {
        serde_yaml::from_str::<serde_yaml::Value>(&text)
            .ok()
            .and_then(|doc| {
                doc.get("scenarios").and_then(|v| v.as_mapping()).map(|m| {
                    m.keys()
                        .filter_map(|k| k.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default()
    } else {
        return Vec::new();
    };
    scenarios.retain(|s| s.starts_with(cur));
    scenarios.sort();
    scenarios
}

fn filter_prefix(opts: &[&str], cur: &str) -> Vec<String> {
    opts.iter()
        .filter(|s| s.starts_with(cur))
        .map(|s| s.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Feature-gated branches: extends CommandTree with optional
// commands without violating the strict-typestate gate.
// ---------------------------------------------------------------------------

trait CommandTreeExt {
    fn with_openapi_commands(self) -> Self;
}

impl CommandTreeExt for CommandTree {
    #[cfg(feature = "openapi")]
    fn with_openapi_commands(self) -> Self {
        self.strict_command(
            "describe-openapi",
            StrictNode::leaf(&[])
                .with_category(Category::Documentation.tag())
                .with_level(Level::FullSurface.rank()),
        )
        .strict_command(
            "run-openapi",
            StrictNode::leaf(&[])
                .with_category(Category::Workloads.tag())
                .with_level(Level::FullSurface.rank()),
        )
    }

    #[cfg(not(feature = "openapi"))]
    fn with_openapi_commands(self) -> Self {
        self
    }
}

// ── cli_spec entry for `nmbrs completions` ─────────────────

/// `nmbrs completions [--shell <name>]` — emit the bash/zsh
/// completion shim or the activation eval line. Walker-parsed.
pub fn spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{
        Arity, Category, Command, Flag, Handler, Level, ParsedCommand, ValueProvider,
    };
    fn shells(p: &str, _: &[&str]) -> Vec<String> {
        ["bash", "zsh", "fish", "elvish", "powershell"]
            .iter()
            .filter(|s| s.starts_with(p))
            .map(|s| s.to_string())
            .collect()
    }
    fn handle(p: ParsedCommand) -> Result<(), String> {
        let mut argv: Vec<String> = Vec::new();
        if let Some(v) = p.flag("--shell") {
            argv.push("--shell".into());
            argv.push(v.into());
        }
        super::completion::print_completions(&argv);
        Ok(())
    }
    Command {
        name: "completions",
        help: "Print shell-completion shim or activation line.",
        category: Category::Shell,
        level: Level::FullSurface,
        flags: vec![Flag {
            long: "--shell",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::Custom(shells),
            help: "bash | zsh | fish | elvish | powershell. Omit for activation line.",
            repeatable: false,
        }],
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: false,
        completion_override: None,
    }
}

#[cfg(test)]
mod walker_tests {
    use super::*;

    /// The value-taking flag set is DERIVED from the spec, not hand-listed.
    ///
    /// It used to be a `matches!` list that had drifted: `--execution` and
    /// `--range` are declared `Arity::Value` in `metrics_cmd` but were absent, so
    /// they neither auto-advanced to value position nor completed their
    /// `--flag=<partial>` form.
    #[test]
    fn value_taking_flags_are_derived_from_the_spec() {
        // Previously missing, and the reason this was found.
        for flag in ["--execution", "--range", "--family", "--by", "--tofile"] {
            assert!(
                flag_takes_value(flag),
                "{flag} is declared Arity::Value in the spec and must be known"
            );
        }
        // Still covers what the hand-written list covered.
        for flag in ["--db", "--session", "--name", "--metric", "--format"] {
            assert!(flag_takes_value(flag), "{flag} regressed out of the set");
        }
        // Bool flags must stay OUT: treating one as value-taking would swallow
        // the following token as its value.
        for flag in [
            "--strict",
            "--all-executions",
            "--no-prompt",
            "--ascii",
            "--per-instance-metrics",
            "--metrics-log",
        ] {
            assert!(
                !flag_takes_value(flag),
                "{flag} takes no value; advertising one would eat the next token"
            );
        }
    }

    /// The derived set must cover everything the retired hand-written list did.
    ///
    /// Deriving is only an improvement if it LOSES nothing: a flag that silently
    /// dropped out would stop auto-advancing to value position, and the loss
    /// would show up as completion "just not working" for that flag. This pins
    /// the retired list verbatim as the floor.
    #[test]
    fn derived_set_covers_the_retired_hand_written_list() {
        const RETIRED: &[&str] = &[
            "--name",
            "--metric",
            "--x",
            "--series",
            "--filter",
            "--db",
            "--output",
            "--label",
            "--palette",
            "--line",
            "--line-width",
            "--marker",
            "--marker-size",
            "--figure-num",
            "--title",
            "--xlabel",
            "--ylabel",
            "--x-scale",
            "--y-scale",
            "--width",
            "--height",
            "--scale",
            "--csv-also",
            "--report",
            "--update-markdown",
            "--add-to-markdown",
            "--format",
            "--create",
            "--session",
            "--session-name",
            "--session-path",
            "--session-reuse",
            "--session-keep",
            "--session-shelflife",
            "--resume",
            "--polydat-lib",
            "--pid",
            "--socket",
        ];
        let missing: Vec<&str> = RETIRED
            .iter()
            .copied()
            .filter(|f| !flag_takes_value(f))
            // `--create` is the one deliberate omission: the retired list had it
            // WRONG. Summary's parser sets `opts.create = true` and reads no
            // value, so counting it as value-taking made completion advance past
            // it and treat the next token — often the spec positional — as its
            // value. Deriving from the spec corrects that.
            .filter(|f| *f != "--create")
            .collect();
        assert!(
            missing.is_empty(),
            "the derived set must not lose flags the hand-written list had: \
             {missing:?}"
        );
        assert!(
            !flag_takes_value("--create"),
            "--create is a boolean; the retired list misclassified it"
        );
    }

    /// `--flag=<partial>` must complete its value, like `--flag <partial>` does.
    #[test]
    fn attached_flag_values_are_split_for_completion() {
        assert_eq!(
            split_attached_value("--execution=7"),
            Some(("--execution".to_string(), "7".to_string()))
        );
        // Empty partial ⇒ offer everything.
        assert_eq!(
            split_attached_value("--execution="),
            Some(("--execution".to_string(), String::new()))
        );
        // A value containing `=` splits only at the FIRST one.
        assert_eq!(
            split_attached_value("--filter=k=v"),
            Some(("--filter".to_string(), "k=v".to_string()))
        );
        // Bool flags and bare params are not this shape.
        assert_eq!(split_attached_value("--strict=1"), None);
        assert_eq!(split_attached_value("session=/tmp/s"), None);
        assert_eq!(split_attached_value("--execution"), None);
    }

    /// The item-name POSITIONAL must complete, and must survive a preceding
    /// `key=value` token.
    ///
    /// Only `--name` carried a provider, so `nmbrs table <TAB>` offered nothing
    /// while `nmbrs table --name <TAB>` listed the very same items — and the
    /// positional is the spelling the docs and everyday use favour. veks counts
    /// every non-`-` word as a positional, so `workload=<file>` alone exhausted a
    /// single slot and silenced the provider even after it was attached.
    #[test]
    fn report_item_names_complete_at_the_positional() {
        // `report_name_slot_open` is the gate; drive it directly since the name
        // sources need a session db or workload on disk.
        assert!(
            report_name_slot_open(&[]),
            "empty line: the name slot is open"
        );
        assert!(
            report_name_slot_open(&["workload=w.yaml"]),
            "a kv param must not consume the name slot"
        );
        assert!(
            report_name_slot_open(&["workload=w.yaml", "session=/tmp/s"]),
            "several kv params must not consume it either"
        );
        assert!(
            report_name_slot_open(&["--db", "/tmp/m.db"]),
            "a value flag and its value must not consume it"
        );
        assert!(
            report_name_slot_open(&["--ascii"]),
            "a bool flag must not consume it"
        );
        assert!(
            report_name_slot_open(&["all"]),
            "a structural word is not the item name"
        );

        // Once a real name is present the provider must go quiet rather than
        // suggest a second one.
        assert!(
            !report_name_slot_open(&["compaction_shape"]),
            "an item name fills the slot"
        );
        assert!(
            !report_name_slot_open(&["workload=w.yaml", "compaction_shape"]),
            "…including after a kv param"
        );
    }

    /// The db-backed value providers must read the session named ON THE LINE.
    ///
    /// All three hardcoded `latest_metrics_db()` while ignoring the `ctx` they
    /// are handed, so completing `--execution` under `--session=<dir>` offered
    /// execution ids from a DIFFERENT session — values that do not exist in the
    /// session being targeted, which is worse than no suggestion. The correct
    /// resolver (`db_path_from_context`, honouring `--db`, `--session*` and bare
    /// `session=`) already existed and had 11 other callers.
    #[test]
    fn db_backed_providers_honour_the_session_named_on_the_line() {
        use nmbrs_metrics::reporters::sqlite::SqliteReporter;
        use nmbrs_metrics::scheduler::Reporter;

        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("nmbrs-provider-ctx-{n:x}"));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("metrics.db");
        {
            let mut reporter = SqliteReporter::new(&db).unwrap();
            // An exec id no real session in the tree would carry, so a value
            // sourced from `sessions/latest` instead cannot pass by luck.
            reporter.insert_execution_start("probe", 77, "run", None, 0, "", "");
            reporter.flush();
        }

        let db_arg = db.to_string_lossy().to_string();
        let got = execution_id_provider("", &["--db", &db_arg]);
        assert!(
            got.iter().any(|e| e == "77"),
            "`--db <path>` must select the db: {got:?}"
        );

        let session_flag = format!("--session={}", dir.display());
        let got = execution_id_provider("", &[&session_flag]);
        assert!(
            got.iter().any(|e| e == "77"),
            "`--session=<dir>` must select the db: {got:?}"
        );

        // The bare spelling resolves identically — read_session_dir routes
        // through the same resolver.
        let bare = format!("session={}", dir.display());
        let got = execution_id_provider("", &[&bare]);
        assert!(
            got.iter().any(|e| e == "77"),
            "bare `session=<dir>` must select the db: {got:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The session provider must scan the root the RUNTIME uses. It read a
    /// hardcoded `"logs"` — the pre-SRD-77 name — so `--session <TAB>` and
    /// `session=<TAB>` silently offered nothing on every current layout, which
    /// reads as "no sessions exist" rather than "this provider is looking in the
    /// wrong place".
    #[test]
    fn session_provider_scans_the_runtime_sessions_root() {
        let root = nmbrs_runtime::session::default_sessions_root();
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("zz-provider-probe-{n:x}");
        std::fs::create_dir_all(root.join(&name)).unwrap();

        let got = session_name_provider("zz-provider-probe-", &[]);
        let _ = std::fs::remove_dir_all(root.join(&name));
        assert!(
            got.contains(&name),
            "provider must list sessions under {root:?}, got {got:?}"
        );
    }

    fn tempdir(tag: &str) -> std::path::PathBuf {
        // /tmp deliberately, not env::temp_dir(): on some setups
        // (e.g. cargo test under TMPDIR=target/test-tmp) the env
        // path lives under `target/` which the walker
        // unconditionally skips, so a tempdir under it would
        // make the walker treat its own root as noise and find
        // nothing.
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::path::PathBuf::from("/tmp").join(format!("nmbrs-completion-{tag}-{n:x}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_yaml(path: &std::path::Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#test\n").unwrap();
    }

    #[test]
    fn walker_finds_yaml_in_nested_subdirs() {
        let root = tempdir("nested");
        write_yaml(&root.join("a.yaml"));
        write_yaml(&root.join("sub/b.yaml"));
        write_yaml(&root.join("sub/deeper/c.yaml"));
        write_yaml(&root.join("sub/deeper/even/d.yaml"));

        let mut out = Vec::new();
        let mut budget = WORKLOAD_MAX_FILES_SCANNED;
        let prefix = format!("{}/", root.display());
        collect_yaml_recursive(&root, &prefix, "", &mut out, &mut budget, 0);
        assert!(out.iter().any(|p| p.ends_with("a.yaml")), "got: {out:?}");
        assert!(
            out.iter().any(|p| p.ends_with("sub/b.yaml")),
            "got: {out:?}"
        );
        assert!(
            out.iter().any(|p| p.ends_with("sub/deeper/c.yaml")),
            "got: {out:?}"
        );
        assert!(
            out.iter().any(|p| p.ends_with("sub/deeper/even/d.yaml")),
            "got: {out:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn walker_stops_at_max_depth() {
        let root = tempdir("depth-cap");
        write_yaml(&root.join("L1/L2/L3/L4/inside4.yaml"));
        write_yaml(&root.join("L1/L2/L3/L4/L5/too_deep.yaml"));

        let mut out = Vec::new();
        let mut budget = WORKLOAD_MAX_FILES_SCANNED;
        let prefix = format!("{}/", root.display());
        collect_yaml_recursive(&root, &prefix, "", &mut out, &mut budget, 0);
        assert!(
            out.iter().any(|p| p.ends_with("L4/inside4.yaml")),
            "depth-4 entry visible: {out:?}"
        );
        assert!(
            !out.iter().any(|p| p.ends_with("too_deep.yaml")),
            "depth-5 entry NOT visible: {out:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn walker_respects_budget_cap() {
        let root = tempdir("budget");
        write_yaml(&root.join("a.yaml"));
        write_yaml(&root.join("sub/b.yaml"));

        let mut out = Vec::new();
        let mut budget: usize = 1;
        let prefix = format!("{}/", root.display());
        collect_yaml_recursive(&root, &prefix, "", &mut out, &mut budget, 0);
        assert_eq!(budget, 0, "budget should be exhausted");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn walker_skips_target_node_modules_logs() {
        let root = tempdir("noise-skip");
        write_yaml(&root.join("good.yaml"));
        write_yaml(&root.join("target/never.yaml"));
        write_yaml(&root.join("node_modules/never.yaml"));
        write_yaml(&root.join("logs/never.yaml"));

        let mut out = Vec::new();
        let mut budget = WORKLOAD_MAX_FILES_SCANNED;
        let prefix = format!("{}/", root.display());
        collect_yaml_recursive(&root, &prefix, "", &mut out, &mut budget, 0);
        assert!(out.iter().any(|p| p.ends_with("good.yaml")));
        assert!(
            !out.iter().any(|p| p.contains("target/")),
            "target/ skipped: {out:?}"
        );
        assert!(
            !out.iter().any(|p| p.contains("node_modules/")),
            "node_modules/ skipped: {out:?}"
        );
        assert!(
            !out.iter().any(|p| p.contains("/logs/")),
            "logs/ skipped: {out:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn walker_filters_top_level_by_name_prefix() {
        let root = tempdir("name-prefix");
        write_yaml(&root.join("alpha.yaml"));
        write_yaml(&root.join("beta.yaml"));
        write_yaml(&root.join("sub/anything.yaml"));

        let mut out = Vec::new();
        let mut budget = WORKLOAD_MAX_FILES_SCANNED;
        let prefix = format!("{}/", root.display());
        collect_yaml_recursive(&root, &prefix, "alp", &mut out, &mut budget, 0);
        assert!(
            out.iter().any(|p| p.ends_with("alpha.yaml")),
            "got: {out:?}"
        );
        assert!(
            !out.iter().any(|p| p.ends_with("beta.yaml")),
            "beta filtered: {out:?}"
        );
        assert!(
            !out.iter().any(|p| p.ends_with("anything.yaml")),
            "non-matching subdir not descended: {out:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // ── SRD-64 §4 — report completion: per-kind subtree contract ──
    //
    // Drives `complete_at_tap` against the published tree and
    // asserts each kind subcommand offers the right vocab flags.

    use veks_completion::complete_at_tap;

    fn complete(words: &[&str]) -> Vec<String> {
        let tree = build_tree();
        // Tap 3 = full surface — `report` is Tap 2, `report
        // <kind>` traversal needs the deepest tier.
        complete_at_tap(&tree, words, 3)
    }

    #[test]
    fn report_lists_all_subcommands() {
        // Only subcommands the parser actually accepts. The
        // pre-cli_spec completion tree advertised `show`,
        // `text`, `file`, `details` too — but `report_command`
        // never handled them as keywords (they fell through to
        // glob-match and errored). Cleaned up so completion
        // doesn't lie about the surface.
        let cands = complete(&["nmbrs", "report", ""]);
        for required in [
            "plot", "table", "list", "all", "figure", "rename", "scratch",
        ] {
            assert!(
                cands.iter().any(|c| c == required),
                "missing subcommand `{required}` in: {cands:?}"
            );
        }
    }

    #[test]
    fn metrics_query_offers_its_flags() {
        // `query` is `raw_args` (parsing is delegated to metricsql_cmd),
        // but its declared flags must still drive completion
        // (cli_spec §raw_args) — this is the gap that left `--lookback`
        // etc. un-completable, plus the new `--range`.
        let cands = complete(&["nmbrs", "metrics", "query", "--"]);
        for required in [
            "--range",
            "--family",
            "--lookback",
            "--at",
            "--step",
            "--stale-window",
            "--all-samples",
            "--db",
        ] {
            assert!(
                cands.iter().any(|c| c == required),
                "missing flag `{required}` for `metrics query` in: {cands:?}"
            );
        }
    }

    #[test]
    fn report_plot_offers_plot_directives() {
        let cands = complete(&["nmbrs", "report", "plot", "demo", "--"]);
        for required in [
            "--over",
            "--by",
            "--where",
            "--agg",
            "--label",
            "--palette",
            "--line",
            "--width",
            "--marker",
            "--size",
            "--color",
            "--metric",
            "--xlabel",
            "--ylabel",
            "--x-scale",
            "--y-scale",
            "--style",
            // Orthogonal dispatch flags.
            "--add",
            "--at",
            "--contextual",
            "--replace",
            "--rename",
            "--group",
            "--workload",
            "--dry-run",
            "--name",
        ] {
            assert!(
                cands.iter().any(|c| c == required),
                "missing flag `{required}` in plot completions: {cands:?}"
            );
        }
    }

    #[test]
    fn report_table_excludes_plot_only_directives() {
        let cands = complete(&["nmbrs", "report", "table", "demo", "--"]);
        for forbidden in [
            "--xlabel",
            "--ylabel",
            "--x-scale",
            "--y-scale",
            "--marker",
            "--line",
            "--width",
            "--size",
        ] {
            assert!(
                !cands.iter().any(|c| c == forbidden),
                "table completions should not offer `{forbidden}` (plot-only): {cands:?}"
            );
        }
        // Data-shape directives still apply to tables.
        for required in [
            "--over",
            "--by",
            "--where",
            "--agg",
            "--metric",
            "--label",
            "--palette",
            "--color",
        ] {
            assert!(
                cands.iter().any(|c| c == required),
                "table completions missing `{required}`: {cands:?}"
            );
        }
    }

    #[test]
    #[ignore = "nmbrs report text/file/details are not currently \
                accepted by the parser; the completion tree no \
                longer advertises them. Re-enable when the SRD-64 \
                flag-form path is extended to non-figure kinds."]
    fn report_text_excludes_figure_directives() {
        let cands = complete(&["nmbrs", "report", "text", "intro", "--"]);
        for forbidden in [
            "--over",
            "--by",
            "--where",
            "--agg",
            "--metric",
            "--x-scale",
            "--y-scale",
            "--marker",
        ] {
            assert!(
                !cands.iter().any(|c| c == forbidden),
                "text completions should not offer `{forbidden}`: {cands:?}"
            );
        }
        for required in ["--label", "--body", "--body-file"] {
            assert!(
                cands.iter().any(|c| c == required),
                "text completions missing `{required}`: {cands:?}"
            );
        }
    }

    #[test]
    fn palette_value_completion_offers_closed_set() {
        let cands = complete(&["nmbrs", "report", "plot", "demo", "--palette", ""]);
        for required in nmbrs_workload::report::vocab::PALETTE_NAMES {
            assert!(
                cands.iter().any(|c| c == required),
                "palette completion missing `{required}`: {cands:?}"
            );
        }
        // Sanity: should NOT offer arbitrary strings.
        assert!(
            !cands.iter().any(|c| c == "nope"),
            "completion shouldn't offer arbitrary values: {cands:?}"
        );
    }

    #[test]
    fn agg_value_completion_offers_closed_set() {
        let cands = complete(&["nmbrs", "report", "plot", "demo", "--agg", ""]);
        for required in nmbrs_workload::report::vocab::AGG_FNS {
            assert!(
                cands.iter().any(|c| c == required),
                "agg completion missing `{required}`: {cands:?}"
            );
        }
    }

    #[test]
    fn x_scale_value_completion_offers_linear_log() {
        let cands = complete(&["nmbrs", "report", "plot", "demo", "--x-scale", ""]);
        assert!(cands.iter().any(|c| c == "linear"));
        assert!(cands.iter().any(|c| c == "log"));
    }

    #[test]
    fn marker_value_completion_offers_shape_set() {
        let cands = complete(&["nmbrs", "report", "plot", "demo", "--marker", ""]);
        for required in [
            "circle", "square", "triangle", "diamond", "plus", "cross", "none",
        ] {
            assert!(
                cands.iter().any(|c| c == required),
                "marker completion missing `{required}`: {cands:?}"
            );
        }
    }

    #[test]
    fn at_anchor_completion_offers_scope_prefixes() {
        let cands = complete(&["nmbrs", "report", "plot", "demo", "--at", ""]);
        for required in ["root", "scenario:", "phase:", "op:"] {
            assert!(
                cands.iter().any(|c| c == required),
                "--at completion missing `{required}`: {cands:?}"
            );
        }
    }

    #[test]
    fn contextual_completion_offers_all_modes() {
        let cands = complete(&["nmbrs", "report", "plot", "demo", "--contextual", ""]);
        for required in ["auto", "root", "scenario", "phase", "op"] {
            assert!(
                cands.iter().any(|c| c == required),
                "--contextual completion missing `{required}`: {cands:?}"
            );
        }
    }

    #[test]
    fn report_scratch_subcommands_listed() {
        let cands = complete(&["nmbrs", "report", "scratch", ""]);
        for required in ["list", "clean", "promote"] {
            assert!(
                cands.iter().any(|c| c == required),
                "scratch subcommand `{required}` missing: {cands:?}"
            );
        }
    }

    #[test]
    fn report_rename_offers_replace_and_dry_run() {
        let cands = complete(&["nmbrs", "report", "rename", "old_name", "new_name", "--"]);
        for required in ["--replace", "--dry-run", "--workload"] {
            assert!(
                cands.iter().any(|c| c == required),
                "rename completion missing `{required}`: {cands:?}"
            );
        }
    }

    #[test]
    fn closed_set_filters_by_partial() {
        // `--palette w` should narrow to palettes starting with `w`.
        let cands = complete(&["nmbrs", "report", "plot", "demo", "--palette", "w"]);
        assert!(cands.iter().any(|c| c == "wong"));
        // Other palettes filtered.
        assert!(
            !cands.iter().any(|c| c == "ibm"),
            "filter should remove non-matching: {cands:?}"
        );
    }
}
