// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-77 — `nmbrs refine`: layer a new execution onto an
//! existing session, running only phases that haven't been
//! completed yet.
//!
//! The verb is a sibling to `run` / `resume`. It re-attaches
//! to a target session (default: `logs/latest`), reads the
//! prior `phase_outcomes` table, and skips any phase whose
//! `(name, labels)` pair already has a completed outcome. The
//! result is "additive workload run" semantics: edit the
//! workload (add a new sweep cell, add a phase) → `nmbrs refine`
//! → only the new work runs.
//!
//! Distinct from `nmbrs run --resume-latest` (today's
//! `resume`) in two ways:
//! - `resume` insists on workload-identity match; `refine`
//!   tolerates additions.
//! - `resume` writes outcomes under the existing `exec_id`;
//!   `refine` bumps to `max(prior) + 1` so the cardinal history
//!   of executions is preserved.
//!
//! Internally `refine_command` augments argv with the markers
//! `nmbrs-runtime::runner` keys off (`--refine` to enable
//! skip-plan loading, `--resume-latest` to point the session
//! resolver at the prior dir) and delegates to the existing
//! `run::run_command` pipeline. The runner's refine branch
//! detects `--refine`, loads the skip plan, picks the next
//! `exec_id`, constructs a `Session::refine`, and threads the
//! plan onto the executor context.

use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};

/// `nmbrs refine` — same flag surface as `run`. Workload args
/// (`key=value`, `--session-path`, …) pass through; the only
/// difference is the verb-driven semantic of "skip prior
/// completed phases, layer a new execution".
pub fn spec() -> Command {
    Command {
        name: "refine",
        help: "Layer a new execution onto an existing session: \
               run phases that are new or haven't completed yet, \
               preserving prior outcomes as history.",
        category: Category::Workloads,
        level: Level::Secondary,
        // Reuse the same flag set as `run`. SRD-77 will add
        // refine-specific flags here (`--scope=missing|changed|all`,
        // `--on-removed=error|keep|drop`) in follow-up pushes;
        // for the MVP, the implicit default scope is `missing`.
        flags: crate::run::standard_run_flags(),
        kv_params: crate::completion::RUN_KV_PARAMS,
        dynamic_options: Some(crate::completion::workload_dynamic_params),
        positionals: Vec::new(),
        subcommands: Vec::new(),
        handler: Some(Handler::Async(refine_handler)),
        raw_args: true,
        completion_override: None,
    }
}

fn refine_handler(
    p: ParsedCommand,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>>>> {
    Box::pin(async move {
        refine_command(&p.raw).await;
        Ok(())
    })
}

/// Run `nmbrs refine` with the given raw argv (everything after
/// the verb token). Augments argv with `--refine` and
/// `--resume-latest` (unless the user explicitly passed
/// `--session` / `--session-path` / `--resume` to target a
/// specific session) and delegates to `run::run_command`.
///
/// `--scope=all` (also accepted as `scope=all` workload-param
/// shorthand) disables the prior-completed-outcome skip plan
/// — every phase runs against the active session, with the
/// new outcomes recorded under the bumped `exec_id`. The prior
/// outcomes stay in the table as cardinal history under their
/// original `exec_id`.
pub async fn refine_command(args: &[String]) {
    // If the user explicitly named a target session via
    // `--session`, `--session-path`, or `--resume`, we honor
    // that — `--resume-latest` would override it. Otherwise
    // default to `--resume-latest` so the implicit target is
    // the most-recent session.
    let explicit_target = args
        .iter()
        .any(|a| a == "--session" || a == "--session-path" || a == "--resume");

    // The run pipeline's argv-shape contract puts the verb
    // token first ("run"); `run_with_observer` strips it
    // before delegating to `run_impl`. Prepend it here so
    // refine threads through the same parser unchanged.
    let mut augmented: Vec<String> = Vec::with_capacity(args.len() + 3);
    augmented.push("run".into());
    augmented.push("--refine".into());
    if !explicit_target {
        augmented.push("--resume-latest".into());
    }
    augmented.extend(args.iter().cloned());

    crate::run::run_command(&augmented).await;
}
