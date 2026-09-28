// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Assembles the root [`Command`] spec by pulling each
//! subcommand's `spec()` from its owning module. main.rs
//! calls [`root`] once at startup; the same value drives the
//! parser, completion, and help.

use super::*;

pub fn root() -> Command {
    Command {
        name: "nmbrs",
        help: "nmbrs — the nmbrs command-line tool.\n\
               \n\
               Run a workload, attach to a running session,\n\
               render reports, query metrics, etc.",
        category: Category::Tools,
        level: Level::Workload,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        handler: None,
        raw_args: false,
        completion_override: None,
        subcommands: vec![
            crate::run::spec(),
            crate::check_cmd::spec(),
            crate::refine::spec(),
            crate::session_cmd::spec(),
            crate::inspector::spec(),
            crate::report_cmd::spec(),
            crate::report_cmd::plot_alias_spec(),
            crate::report_cmd::table_alias_spec(),
            crate::metrics_cmd::spec(),
            crate::describe::spec(),
            crate::blueprint_cmd::spec(),
            crate::copy_cmd::spec(),
            crate::diag_cmd::spec(),
            crate::bench::spec(),
            crate::replay::spec(),
            crate::checkpoint_cmd::spec(),
            crate::daemon::spec(),
            crate::plot::spec(),
            crate::completion::spec(),
            #[cfg(feature = "openapi")]
            crate::openapi::describe_spec(),
            #[cfg(feature = "openapi")]
            crate::openapi::run_spec(),
        ],
    }
}
