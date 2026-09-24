// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `nmbrs session` — session lifecycle management.
//!
//! `nmbrs session init` creates a new, empty session (its directory,
//! `metrics.db` schema, and the invariant `session` metadata) WITHOUT
//! running any workload. A later `nmbrs run` / `nmbrs refine` attaches
//! the first execution. The session name can be given literally
//! (`name=`) or built from a template (`template=`) that substitutes
//! datetime fields and accepts polydat string form.

use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
use nmbrs_runtime::session::{SessionReuse, init_empty_session, utc_datetime_fields};

/// Spec for the `session` command tree.
/// kv forms `handle_init` consumes.
const INIT_KV: &[crate::cli_spec::KvParam] = &[
    crate::cli_spec::KvParam {
        key: "name=",
        provider: crate::completion::free_form,
    },
    crate::cli_spec::KvParam {
        key: "template=",
        provider: crate::completion::free_form,
    },
    crate::cli_spec::KvParam {
        key: "session-path=",
        provider: crate::completion::free_form,
    },
    crate::cli_spec::KvParam {
        key: "session-reuse=",
        provider: crate::completion::session_reuse_values,
    },
];

pub fn spec() -> Command {
    Command {
        name: "session",
        help: "Manage session lifecycle (init).",
        category: Category::Tools,
        level: Level::Secondary,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        handler: Some(Handler::Sync(handle_bare)),
        raw_args: false,
        completion_override: None,
        subcommands: vec![Command {
            name: "init",
            help: "Initialize a new, empty session without running anything.",
            category: Category::Tools,
            level: Level::Secondary,
            flags: Vec::new(),
            kv_params: INIT_KV,
            dynamic_options: None,
            positionals: Vec::new(),
            handler: Some(Handler::Sync(handle_init)),
            raw_args: true,
            completion_override: None,
            subcommands: Vec::new(),
        }],
    }
}

fn handle_bare(_p: ParsedCommand) -> Result<(), String> {
    eprintln!(
        "usage: nmbrs session init [name=<name> | template=<template>] \
         [session-path=<dir>] [session-reuse=error|restart|resume]\n\
         \n  template fields (UTC): year month day hour minute second \
         date(YYYYMMDD) time(HHMMSS) datetime(YYYYMMDD_HHMMSS) \
         epoch_millis epoch_seconds\n  \
         e.g.  nmbrs session init template=bench_{{date}}_{{time}}"
    );
    Ok(())
}

fn handle_init(p: ParsedCommand) -> Result<(), String> {
    let mut name: Option<String> = None;
    let mut template: Option<String> = None;
    let mut path: Option<String> = None;
    let mut reuse = SessionReuse::Error;

    let mut it = p.raw.iter();
    while let Some(arg) = it.next() {
        if let Some(v) = arg.strip_prefix("name=") {
            name = Some(v.to_string());
        } else if let Some(v) = arg.strip_prefix("template=") {
            template = Some(v.to_string());
        } else if let Some(v) = arg.strip_prefix("session-path=") {
            path = Some(v.to_string());
        } else if let Some(v) = arg.strip_prefix("session-reuse=") {
            reuse = SessionReuse::parse(v)?;
        } else if arg == "--session-path" {
            path = it.next().cloned();
        } else if arg == "--session-reuse" {
            reuse = SessionReuse::parse(it.next().map(String::as_str).unwrap_or(""))?;
        } else {
            return Err(format!(
                "session init: unexpected argument '{arg}' \
                 (expected name=, template=, session-path=, session-reuse=)"
            ));
        }
    }

    if name.is_some() && template.is_some() {
        return Err("session init: give either name= or template=, not both".to_string());
    }

    // Resolve the session id. `name` is literal; `template` (and the
    // no-arg default) goes through the datetime/polydat templater.
    let id = match (name, template) {
        (Some(n), _) => n,
        (_, Some(t)) => eval_name_template(&t)?,
        (None, None) => eval_name_template("session_{datetime}")?,
    };
    validate_session_id(&id)?;

    let dir = init_empty_session(&id, path.as_deref().map(std::path::Path::new), reuse)?;
    println!("initialized session '{id}' at {}", dir.display());
    Ok(())
}

/// Evaluate a session-name template. The template is the BODY of a
/// polydat string literal: literal text passes through, while `{...}`
/// interpolations reference the bound datetime fields — `year`,
/// `month`, `day`, `hour`, `minute`, `second` (zero-padded), `date`
/// (`YYYYMMDD`), `time` (`HHMMSS`), `datetime` (`YYYYMMDD_HHMMSS`),
/// `epoch_millis`, `epoch_seconds` — or any polydat expression over
/// them. So `bench_{date}` and `run_{date}_v{epoch_millis % 1000}`
/// both work. All fields are UTC.
fn eval_name_template(template: &str) -> Result<String, String> {
    let (y, mo, d, h, mi, s, ems, es) = utc_datetime_fields();
    let fields = format!(
        "const year := \"{y:04}\"\nconst month := \"{mo:02}\"\nconst day := \"{d:02}\"\n\
         const hour := \"{h:02}\"\nconst minute := \"{mi:02}\"\nconst second := \"{s:02}\"\n\
         const date := \"{y:04}{mo:02}{d:02}\"\nconst time := \"{h:02}{mi:02}{s:02}\"\n\
         const datetime := \"{y:04}{mo:02}{d:02}_{h:02}{mi:02}{s:02}\"\n\
         const epoch_millis := {ems}\nconst epoch_seconds := {es}\n",
    );
    // The template is a polydat string-literal body; escape so the
    // user's text can't break out of the surrounding quotes.
    let escaped = template.replace('\\', "\\\\").replace('"', "\\\"");
    let source = format!("{fields}out := \"{escaped}\"");
    let mut kernel = polydat::dsl::compile::compile_polydat(&source)
        .map_err(|e| format!("session name template '{template}': {e}"))?;
    match kernel.pull("out") {
        polydat::ast::Value::Str(s) => Ok(s.to_string()),
        polydat::ast::Value::None => Err(format!(
            "session name template '{template}' produced no value"
        )),
        other => Err(format!(
            "session name template '{template}' must produce a string, got {other:?}"
        )),
    }
}

/// Reject ids that aren't a single, filesystem-safe path component.
fn validate_session_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("session id is empty".to_string());
    }
    if id.contains('/') || id.contains('\\') || id == "." || id == ".." {
        return Err(format!("session id '{id}' must be a single path component"));
    }
    if id.chars().any(char::is_control) {
        return Err(format!("session id '{id}' contains control characters"));
    }
    Ok(())
}
