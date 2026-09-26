// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

// Module-level allow: the legacy `LIST_FLAGS` / `MATCH_FLAGS` /
// `SESSION_FLAGS` constants and the corresponding `*_all_flags`
// helpers + `metrics_command` are referenced only by
// `completion::metrics_node()`, which itself is in the legacy
// dead path retained for fallback (see `completion.rs` module
// comment). Kept so a future re-activation of the legacy tree
// finds the contracts intact.
#![allow(dead_code)]

//! `nmbrs metrics list [<expr>]` and `nmbrs metrics show [<expr>]`
//!
//! Reads `metric_instance` rows from the active session's
//! `metrics.db` and renders them as a hierarchical tree keyed
//! on metric family then label dimensions in declaration order.
//! `show` adds a one-line summary of the most recent
//! `sample_value` row per instance.
//!
//! Filter expression accepts:
//!   - bare glob on the family name: `recall*`
//!   - OpenMetrics-style label match: `recall@10.mean{k="10"}`
//!   - substring filter via `~`: `{profile=~label}` or
//!     `recall{profile=~label}`
//!
//! Honors `--session <path>` / `--session-name` / `--session-path`, and the bare
//! `session=<path>` spelling, resolved locally by `resolve_db`; the active db
//! defaults to `sessions/latest/metrics.db`. `--db <path>` overrides all of them.
//!
//! These used to be swallowed here and applied by a startup hook that repointed
//! the `sessions/latest` symlink — so a read mutated shared state, and only
//! sessions under `sessions/` worked at all.
//!
//! Output formats (`--format <name>` on `list` / `show`):
//!   - `plain`       hierarchical tree (default)
//!   - `json`        single pretty-printed object envelope
//!   - `jsonl`       one JSON object per instance, no envelope
//!   - `yaml`        single YAML document
//!   - `csv`         table with the union of label keys as columns
//!
//! Sink: `--tofile <path>`. When `--format` is omitted, the
//! file extension chooses (`.json`, `.jsonl`, `.yaml` / `.yml`,
//! `.csv`, `.txt`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Session-resolution flags consumed by the umbrella before
/// the subcommand parser sees them. Listed here so the
/// completion tree can offer them without re-typing the set.
pub const SESSION_FLAGS: &[&str] = &[
    "--session",
    "--session-name",
    "--session-path",
    "--session-reuse",
    "--session-keep",
    "--session-shelflife",
];

/// Canonical flag set for `nmbrs metrics list` and `nmbrs
/// metrics show`. Single source of truth — both this file's
/// parser and `completion::metrics_node` read from here, so
/// adding a flag is one edit, not two.
pub const LIST_FLAGS: &[&str] = &["--db", "--format", "--tofile"];

/// Boolean flags for `nmbrs metrics show`. Same single-source-of-truth
/// contract as [`LIST_FLAGS`], but these don't take a value. `--list` is
/// the consolidated former `list` subcommand: it drops the per-leaf value
/// summary, leaving just the family + label-dimension tree.
pub const LIST_BOOL_FLAGS: &[&str] = &["--tree", "--list"];

/// Canonical flag set for `nmbrs metrics match`.
pub const MATCH_FLAGS: &[&str] = &["--db"];

/// Closed value set for `--format`. `metricsql` is the single canonical
/// name for the OpenMetrics/PromQL selector line form (`family{labels}`) —
/// the former `openmetrics` / `promql` / `prometheus` synonyms are dropped
/// (they produced byte-identical output; one name, no aliases).
pub const FORMAT_VALUES: &[&str] = &["plain", "json", "jsonl", "yaml", "csv", "metricsql"];

/// Concatenation helper: the full advertised flag list for a
/// given subcommand (kind-specific flags + session flags).
pub fn list_all_flags() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = LIST_FLAGS.to_vec();
    v.extend(SESSION_FLAGS.iter().copied());
    v
}

pub fn match_all_flags() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = MATCH_FLAGS.to_vec();
    v.extend(SESSION_FLAGS.iter().copied());
    v
}

// ── cli_spec entry ─────────────────────────────────────────
// Single source of truth for `nmbrs metrics …` shape: the
// walker-driven parser, completion, and help all read this.
// Adding a flag here automatically surfaces in tab and in
// `nmbrs metrics … --help`.

use crate::cli_spec::{
    Arity, Category, Command, Flag, Handler, Level, ParsedCommand, ValueProvider,
};

/// Closed-set value provider for `--format`. Returned values
/// match [`Format::parse`].
fn format_completer(partial: &str, _ctx: &[&str]) -> Vec<String> {
    FORMAT_VALUES
        .iter()
        .filter(|s| s.starts_with(partial))
        .map(|s| s.to_string())
        .collect()
}

/// SRD-77 — the `--execution=<n>` / `--all-executions` pair
/// every `nmbrs metrics` subcommand accepts. Declared once
/// here so the spec stays consistent across `list` / `show` /
/// `match` / `groups` and the completion surface offers the
/// same flags on each.
fn execution_qualifier_flags() -> Vec<Flag> {
    crate::completion::execution_flags()
}

/// The session-resolution flags every read-side metrics subcommand accepts.
///
/// Declared in the spec, not just swallowed by the parsers: the walker validates
/// against the spec, so an undeclared flag is REJECTED — `nmbrs metrics groups
/// --session=<dir>` failed with "unknown flag '--session'" even though the parser
/// had a branch for it. The lifecycle members of the family
/// (`--session-reuse/keep/shelflife`) are deliberately absent: they choose how to
/// treat a session being written, and these commands only read.
/// The bare `session=<dir>` spelling, declared so completion offers it beside
/// `--session`. The handlers accept it via `forward_session_flags`; declaring it
/// here is what makes it discoverable rather than folklore.
pub static SESSION_KV: &[crate::cli_spec::KvParam] = &[crate::completion::SESSION_KV_PARAM];

fn session_resolution_flags() -> Vec<Flag> {
    vec![
        Flag {
            long: "--session",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: crate::completion::SESSION_NAME_VALUE,
            help: "Read this session instead of the latest (path or name).",
            repeatable: false,
        },
        Flag {
            long: "--session-name",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: crate::completion::SESSION_NAME_VALUE,
            help: "Read the session with this name.",
            repeatable: false,
        },
        Flag {
            long: "--session-path",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::Path,
            help: "Read the session at this path.",
            repeatable: false,
        },
    ]
}

fn list_or_show_flags() -> Vec<Flag> {
    let mut out = vec![
        crate::completion::db_flag(),
        Flag {
            long: "--format",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::Custom(format_completer),
            help: "plain | json | jsonl | yaml | csv. Default: plain.",
            repeatable: false,
        },
        Flag {
            long: "--tofile",
            short: None,
            aliases: &["--to-file"],
            arity: Arity::Value,
            value: ValueProvider::Path,
            help: "Write to <path>. With no --format, extension picks.",
            repeatable: false,
        },
        Flag {
            long: "--tree",
            short: None,
            aliases: &[],
            arity: Arity::Bool,
            value: ValueProvider::None,
            help: "Reshape json/yaml as nested key=value tree.",
            repeatable: false,
        },
        Flag {
            long: "--list",
            short: None,
            aliases: &[],
            arity: Arity::Bool,
            value: ValueProvider::None,
            help: "Names only — families + label dimensions, no value summary \
                   (the consolidated former `metrics list`).",
            repeatable: false,
        },
    ];
    out.push(Flag {
        long: "--scope",
        short: None,
        aliases: &[],
        arity: Arity::Bool,
        value: ValueProvider::None,
        help: "Annotate each leaf with its SRD-93 lifecycle state \
               (in-scope / exited / no clean exit) from the scope-event \
               table. Plain format only; silent on pre-SRD-93 dbs.",
        repeatable: false,
    });
    out.extend(execution_qualifier_flags());
    out.extend(session_resolution_flags());
    out
}

/// `list`'s flag surface: the shared flags minus `--list` — a names
/// view of a names view is a no-op, so completion doesn't offer it
/// (the imperative parser still accepts it for old scripts).
fn list_cmd_flags() -> Vec<Flag> {
    list_or_show_flags()
        .into_iter()
        .filter(|f| f.long != "--list")
        .collect()
}

/// The deprecated `show` alias's flag surface: `list`'s flags plus
/// the one-release `--values` bridge to `summarize`.
fn show_flags() -> Vec<Flag> {
    let mut out = list_or_show_flags();
    out.push(Flag {
        long: "--values",
        short: None,
        aliases: &[],
        arity: Arity::Bool,
        value: ValueProvider::None,
        help: "DEPRECATED — per-leaf value summaries moved to \
               `nmbrs metrics summarize`.",
        repeatable: false,
    });
    out
}

/// `watch`'s flag surface — exactly what
/// `metricsql_cmd::parse_watch_args` consumes.
fn watch_flags() -> Vec<Flag> {
    let mut out = vec![
        crate::completion::db_flag(),
        Flag {
            long: "--interval",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "Polling interval (e.g. 1s, 500ms). Default: 2s.",
            repeatable: false,
        },
        Flag {
            long: "--warmup",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "Delay before the first poll.",
            repeatable: false,
        },
        Flag {
            long: "--all-samples",
            short: None,
            aliases: &[],
            arity: Arity::Bool,
            value: ValueProvider::None,
            help: "Evaluate over every sample, not the live window.",
            repeatable: false,
        },
        Flag {
            long: "--no-clear",
            short: None,
            aliases: &[],
            arity: Arity::Bool,
            value: ValueProvider::None,
            help: "Append output instead of clearing the screen per poll.",
            repeatable: false,
        },
    ];
    out.extend(session_resolution_flags());
    out
}

fn match_flags() -> Vec<Flag> {
    let mut out = vec![Flag {
        long: "--db",
        short: None,
        aliases: &[],
        arity: Arity::Value,
        value: ValueProvider::Path,
        help: "Override metrics db. Default: sessions/latest/metrics.db",
        repeatable: false,
    }];
    out.extend(execution_qualifier_flags());
    out.extend(session_resolution_flags());
    out
}

/// Flags for `nmbrs metrics query`. The runtime parse lives in
/// `metricsql_cmd::parse_args` (the `query` cli_spec entry is `raw_args`),
/// but declaring the flags here drives completion — the spec stays the
/// source of truth (cli_spec §`raw_args`). Keep in sync with
/// `metricsql_cmd::parse_args`.
fn query_flags() -> Vec<Flag> {
    let mut out = vec![
        crate::completion::db_flag(),
        Flag {
            long: "--range",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::Custom(crate::completion::metrics_range_provider),
            help: "Apply a metricsql `[<dur>]` range to the selector: `all` = \
                   the whole test span, or a reporting cadence (e.g. 1m).",
            repeatable: false,
        },
        Flag {
            long: "--family",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::Custom(crate::completion::metric_family_provider),
            help: "Add a metric family to the query (repeatable; tab-completes \
                   to family names). Each is merged into one output — select \
                   several families at once.",
            repeatable: true,
        },
        Flag {
            long: "--at",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "Anchor timestamp (ms epoch). Default: the db's latest sample.",
            repeatable: false,
        },
        Flag {
            long: "--lookback",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "Range-query lookback window (stepped). Default: 0 = instant.",
            repeatable: false,
        },
        Flag {
            long: "--step",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "Range-query step. Default: 1m.",
            repeatable: false,
        },
        Flag {
            long: "--stale-window",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "Instant-query staleness window. Default: 5m.",
            repeatable: false,
        },
        Flag {
            long: "--all-samples",
            short: None,
            aliases: &[],
            arity: Arity::Bool,
            value: ValueProvider::None,
            help: "Emit every sample, not just the latest per series.",
            repeatable: false,
        },
    ];
    out.extend(session_resolution_flags());
    out
}

pub fn spec() -> Command {
    // No-subcommand handler: print usage. The walker lands
    // here when the user types `nmbrs metrics` or
    // `nmbrs metrics --bogus` — `raw_args: true` lets us absorb
    // any tail and the handler decides how to react.
    fn handle_bare(_p: ParsedCommand) -> Result<(), String> {
        print_metrics_usage();
        Ok(())
    }
    Command {
        name: "metrics",
        help: "Read-side introspection over a session's metrics db.",
        category: Category::Tools,
        level: Level::Secondary,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        handler: Some(Handler::Sync(handle_bare)),
        raw_args: true,
        completion_override: None,
        subcommands: vec![
            Command {
                name: "list",
                help: "List metric families + label dimensions (structure \
                       only, never touches sample data — SRD-93). For \
                       per-leaf value summaries use `summarize`.",
                category: Category::Tools,
                level: Level::Secondary,
                flags: list_cmd_flags(),
                kv_params: SESSION_KV,
                dynamic_options: None,
                positionals: vec![crate::cli_spec::Positional {
                    name: "expr",
                    help: "Optional filter (family-glob or `family{labels}`).",
                    value: crate::cli_spec::ValueProvider::Custom(
                        crate::completion::metric_family_provider,
                    ),
                    kind: crate::cli_spec::PositionalKind::ZeroOrOne,
                }],
                subcommands: Vec::new(),
                handler: Some(Handler::Sync(handle_list)),
                raw_args: false,
                completion_override: None,
            },
            Command {
                name: "show",
                help: "DEPRECATED alias of `list` (SRD-93 renamed the \
                       structure view); `--values` bridges to `summarize`.",
                category: Category::Tools,
                level: Level::Secondary,
                flags: show_flags(),
                kv_params: SESSION_KV,
                dynamic_options: None,
                positionals: vec![crate::cli_spec::Positional {
                    name: "expr",
                    help: "Optional filter (family-glob or `family{labels}`).",
                    value: crate::cli_spec::ValueProvider::Custom(
                        crate::completion::metric_family_provider,
                    ),
                    kind: crate::cli_spec::PositionalKind::ZeroOrOne,
                }],
                subcommands: Vec::new(),
                handler: Some(Handler::Sync(handle_show)),
                raw_args: false,
                completion_override: None,
            },
            Command {
                name: "summarize",
                help: "Metric families + label dimensions with a value \
                       summary at each leaf (one pass over the sample \
                       data); `--list` omits the values.",
                category: Category::Tools,
                level: Level::Secondary,
                flags: list_or_show_flags(),
                kv_params: SESSION_KV,
                dynamic_options: None,
                positionals: vec![crate::cli_spec::Positional {
                    name: "expr",
                    help: "Optional filter (family-glob or `family{labels}`).",
                    value: crate::cli_spec::ValueProvider::Custom(
                        crate::completion::metric_family_provider,
                    ),
                    kind: crate::cli_spec::PositionalKind::ZeroOrOne,
                }],
                subcommands: Vec::new(),
                handler: Some(Handler::Sync(handle_summarize)),
                raw_args: false,
                completion_override: None,
            },
            Command {
                name: "last",
                help: "Last recorded sample per matching instance \
                       (SRD-93 M6) with lifecycle context.",
                category: Category::Tools,
                level: Level::Secondary,
                flags: match_flags(),
                kv_params: SESSION_KV,
                dynamic_options: None,
                positionals: vec![crate::cli_spec::Positional {
                    name: "expr",
                    help: "Optional filter (family-glob or `family{labels}`).",
                    value: crate::cli_spec::ValueProvider::Custom(
                        crate::completion::metric_family_provider,
                    ),
                    kind: crate::cli_spec::PositionalKind::ZeroOrOne,
                }],
                subcommands: Vec::new(),
                handler: Some(Handler::Sync(handle_last)),
                raw_args: false,
                completion_override: None,
            },
            Command {
                name: "match",
                help: "Flat list of `family{labels}` specs matching <expr>.",
                category: Category::Tools,
                level: Level::Secondary,
                flags: match_flags(),
                kv_params: SESSION_KV,
                dynamic_options: None,
                positionals: vec![crate::cli_spec::Positional {
                    name: "expr",
                    help: "Filter expression (required).",
                    value: crate::cli_spec::ValueProvider::Custom(
                        crate::completion::metric_family_provider,
                    ),
                    kind: crate::cli_spec::PositionalKind::One,
                }],
                subcommands: Vec::new(),
                handler: Some(Handler::Sync(handle_match)),
                raw_args: false,
                completion_override: None,
            },
            Command {
                name: "groups",
                help: "Pivot matching instances into a per-label-tuple table \
                       (instances, rows, mean, min, max).",
                category: Category::Tools,
                level: Level::Secondary,
                flags: groups_flags(),
                kv_params: SESSION_KV,
                dynamic_options: None,
                positionals: vec![crate::cli_spec::Positional {
                    name: "expr",
                    help: "Filter expression (`family{labels}`); required.",
                    value: crate::cli_spec::ValueProvider::Custom(
                        crate::completion::metric_family_provider,
                    ),
                    kind: crate::cli_spec::PositionalKind::One,
                }],
                subcommands: Vec::new(),
                handler: Some(Handler::Sync(handle_groups)),
                raw_args: false,
                completion_override: None,
            },
            // metricsql query/watch keep their existing parsers
            // (they have a richer flag surface — duration, --at,
            // streaming options). raw_args=true here so the spec
            // documents the subcommand without the walker
            // attempting to second-guess metricsql_cmd's parser.
            Command {
                name: "query",
                help: "Evaluate a metricsql expression against the db.",
                category: Category::Tools,
                level: Level::Secondary,
                // `raw_args` delegates parsing to `metricsql_cmd`, but the
                // declared flags still drive completion (cli_spec §raw_args)
                // — so `--range`/`--lookback`/… now tab-complete.
                flags: query_flags(),
                kv_params: SESSION_KV,
                dynamic_options: None,
                // Completion-only positional: `raw_args=true` makes the
                // walker hand the unparsed tail straight to
                // `metricsql_cmd::query` (see walker.rs — the raw-args
                // branch returns before positionals are read), so this
                // entry never affects runtime parsing. It exists so
                // `nmbrs metrics query <TAB>` completes the first token to
                // a metric family name, matching the `list`/`show`/`match`
                // siblings.
                positionals: vec![crate::cli_spec::Positional {
                    name: "expr",
                    help: "metricsql expression; first token completes to a metric family name.",
                    value: crate::cli_spec::ValueProvider::Custom(
                        crate::completion::metric_family_provider,
                    ),
                    kind: crate::cli_spec::PositionalKind::ZeroOrOne,
                }],
                subcommands: Vec::new(),
                handler: Some(Handler::Sync(handle_query)),
                raw_args: true,
                completion_override: None,
            },
            Command {
                name: "watch",
                help: "Live-update a metricsql expression on a polling interval.",
                category: Category::Tools,
                level: Level::Secondary,
                flags: watch_flags(),
                kv_params: SESSION_KV,
                dynamic_options: None,
                // Completion-only positional — see the `query` note above.
                positionals: vec![crate::cli_spec::Positional {
                    name: "expr",
                    help: "metricsql expression; first token completes to a metric family name.",
                    value: crate::cli_spec::ValueProvider::Custom(
                        crate::completion::metric_family_provider,
                    ),
                    kind: crate::cli_spec::PositionalKind::ZeroOrOne,
                }],
                subcommands: Vec::new(),
                handler: Some(Handler::Sync(handle_watch)),
                raw_args: true,
                completion_override: None,
            },
        ],
    }
}

// ── handlers ──────────────────────────────────────────────

fn handle_list(p: ParsedCommand) -> Result<(), String> {
    // SRD-93 stage 1/5 — `list` is structure-only: families + label
    // dimensions from the naming scaffold, never touching
    // `sample_value`. Value summaries live in `summarize`.
    list_from_parsed(&p, false);
    Ok(())
}

fn handle_show(p: ParsedCommand) -> Result<(), String> {
    // Deprecated alias of `list` (SRD-93 renamed the structure
    // view); `--values` bridges one release to `summarize`.
    let values = p.bool("--values");
    if values {
        eprintln!(
            "nmbrs metrics: `show --values` is deprecated — \
             use `nmbrs metrics summarize`."
        );
    } else {
        eprintln!(
            "nmbrs metrics: `show` is deprecated — \
             use `nmbrs metrics list`."
        );
    }
    list_from_parsed(&p, values);
    Ok(())
}

fn handle_summarize(p: ParsedCommand) -> Result<(), String> {
    // Value summaries at each leaf, computed in one pass over the
    // sample data; `--list` drops the values (forwarded, flips
    // `show_values` off inside `list()`).
    list_from_parsed(&p, true);
    Ok(())
}

fn handle_last(p: ParsedCommand) -> Result<(), String> {
    let mut argv: Vec<String> = Vec::new();
    if let Some(db) = p.flag("--db") {
        argv.push("--db".into());
        argv.push(db.to_string());
    }
    forward_exec_qualifier_flags(&p, &mut argv);
    forward_session_flags(&p, &mut argv);
    if let Some(expr) = expr_positional(&p) {
        argv.push(expr.to_string());
    }
    last_specs(&argv);
    Ok(())
}

fn handle_groups(p: ParsedCommand) -> Result<(), String> {
    let mut argv: Vec<String> = Vec::new();
    if let Some(db) = p.flag("--db") {
        argv.push("--db".into());
        argv.push(db.to_string());
    }
    if let Some(by) = p.flag("--by") {
        argv.push("--by".into());
        argv.push(by.to_string());
    }
    if let Some(fmt) = p.flag("--format") {
        argv.push("--format".into());
        argv.push(fmt.to_string());
    }
    forward_exec_qualifier_flags(&p, &mut argv);
    forward_session_flags(&p, &mut argv);
    if let Some(expr) = expr_positional(&p) {
        argv.push(expr.to_string());
    }
    groups_command(&argv);
    Ok(())
}

/// SRD-77 — forward the `--execution=<n>` / `--all-executions`
/// pair from the walker-parsed command into the legacy
/// imperative argv shape every metrics subcommand still
/// consumes. Single chokepoint so the pair never drops on
/// the floor at one of the handlers.
/// Forward the session-resolution flags into the legacy parser's argv.
///
/// Each handler rebuilds an argv for its imperative parser, and these were simply
/// LEFT OUT — so `--session` was accepted by the walker, then dropped before
/// `resolve_db` could see it, and the command silently read the latest session
/// instead of the named one. Declaring a flag and forwarding it are two separate
/// obligations; satisfying only the first is what makes a flag look supported.
fn forward_session_flags(p: &ParsedCommand, argv: &mut Vec<String>) {
    for flag in ["--session", "--session-name", "--session-path"] {
        if let Some(v) = p.flag(flag) {
            argv.push(flag.to_string());
            argv.push(v.to_string());
        }
    }
    // The bare `session=<dir>` spelling too. On a walker-parsed command it
    // arrives as a POSITIONAL, so without this it was silently swallowed as (or
    // beside) the expression and the command read the latest session while naming
    // another — the same silent-wrong-answer the `--session` gap produced. The
    // report/summary family already accepts this spelling.
    if let Some(v) = bare_session_positional(p) {
        argv.push("--session".to_string());
        argv.push(v);
    }
}

/// The value of a bare `session=<dir>` positional, if present.
fn bare_session_positional(p: &ParsedCommand) -> Option<String> {
    p.positionals
        .iter()
        .find_map(|t| t.strip_prefix("session=").map(|v| v.to_string()))
        .filter(|v| !v.is_empty())
}

/// The first positional that is the command's EXPRESSION — skipping a bare
/// `session=<dir>`, which is a parameter rather than an expression. Without this,
/// `metrics match session=<dir> "pat"` would take the session token as the
/// pattern and match nothing.
fn expr_positional(p: &ParsedCommand) -> Option<&str> {
    p.positionals
        .iter()
        .map(|s| s.as_str())
        .find(|t| !t.starts_with("session="))
}

fn forward_exec_qualifier_flags(p: &ParsedCommand, argv: &mut Vec<String>) {
    if let Some(n) = p.flag("--execution") {
        argv.push("--execution".into());
        argv.push(n.to_string());
    }
    if p.bool("--all-executions") {
        argv.push("--all-executions".into());
    }
}

fn groups_flags() -> Vec<Flag> {
    let mut out = vec![
        crate::completion::db_flag(),
        Flag {
            long: "--by",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "Comma-separated label keys to group by (required).",
            repeatable: false,
        },
        Flag {
            long: "--format",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::Custom(format_completer),
            help: "plain | csv | json. Default: plain.",
            repeatable: false,
        },
    ];
    out.extend(execution_qualifier_flags());
    out.extend(session_resolution_flags());
    out
}

fn handle_match(p: ParsedCommand) -> Result<(), String> {
    // Reconstruct an arg list compatible with the existing
    // `match_specs` parser. Cheaper than rewriting the
    // imperative match_specs body to consume ParsedCommand
    // directly — both surfaces share the same flag set, so
    // the walker has already validated unknown flags upstream.
    let mut argv: Vec<String> = Vec::new();
    if let Some(db) = p.flag("--db") {
        argv.push("--db".into());
        argv.push(db.to_string());
    }
    forward_exec_qualifier_flags(&p, &mut argv);
    forward_session_flags(&p, &mut argv);
    if let Some(expr) = expr_positional(&p) {
        argv.push(expr.to_string());
    }
    match_specs(&argv);
    Ok(())
}

fn handle_query(p: ParsedCommand) -> Result<(), String> {
    crate::metricsql_cmd::query(&p.raw);
    Ok(())
}

fn handle_watch(p: ParsedCommand) -> Result<(), String> {
    crate::metricsql_cmd::watch(&p.raw);
    Ok(())
}

/// Walker-driven entry into the rendering pipeline. Reads
/// every option from [`ParsedCommand`] — the walker has
/// already accepted the flag set against
/// [`list_or_show_flags`] so this body never touches argv
/// strings.
fn list_from_parsed(p: &ParsedCommand, show_values: bool) {
    let mut argv: Vec<String> = Vec::new();
    if let Some(db) = p.flag("--db") {
        argv.push("--db".into());
        argv.push(db.to_string());
    }
    if let Some(fmt) = p.flag("--format") {
        argv.push("--format".into());
        argv.push(fmt.to_string());
    }
    if let Some(file) = p.flag("--tofile") {
        argv.push("--tofile".into());
        argv.push(file.to_string());
    }
    if p.bool("--tree") {
        argv.push("--tree".into());
    }
    if p.bool("--list") {
        argv.push("--list".into());
    }
    if p.bool("--scope") {
        argv.push("--scope".into());
    }
    forward_exec_qualifier_flags(p, &mut argv);
    forward_session_flags(p, &mut argv);
    if let Some(expr) = expr_positional(p) {
        argv.push(expr.to_string());
    }
    list(&argv, show_values);
}

pub fn metrics_command(args: &[String]) {
    let sub = args.first().map(String::as_str);
    let rest = args.get(1..).unwrap_or(&[]);
    match sub {
        // SRD-93: `list` is the structure view (`show` is its
        // deprecated alias); `summarize` carries the per-leaf
        // value summaries.
        Some("list") => list(rest, false),
        Some("show") => {
            eprintln!(
                "nmbrs metrics: `show` is deprecated — \
                       use `nmbrs metrics list`."
            );
            list(rest, false)
        }
        Some("summarize") => list(rest, true),
        Some("last") => last_specs(rest),
        Some("match") => match_specs(rest),
        Some("groups") => groups_command(rest),
        Some("query") => crate::metricsql_cmd::query(rest),
        Some("watch") => crate::metricsql_cmd::watch(rest),
        Some(other) => {
            eprintln!("nmbrs metrics: unknown subcommand '{other}'");
            print_metrics_usage();
            std::process::exit(2);
        }
        None => {
            eprintln!("nmbrs metrics: missing subcommand");
            print_metrics_usage();
            std::process::exit(2);
        }
    }
}

fn print_metrics_usage() {
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  nmbrs metrics list  [<expr>]  Metric families + label dimensions");
    eprintln!("                               (structure only — fast on any db,");
    eprintln!("                               live or finished). Add --tree for");
    eprintln!("                               a nested view. (`show` is a");
    eprintln!("                               deprecated alias.)");
    eprintln!("  nmbrs metrics summarize [<expr>]");
    eprintln!("                               Same layout with a value summary");
    eprintln!("                               at each leaf, computed in one");
    eprintln!("                               pass over the sample data. Add");
    eprintln!("                               --list for names only.");
    eprintln!("  nmbrs metrics last  [<expr>]  Last recorded sample per");
    eprintln!("                               matching instance, with the");
    eprintln!("                               SRD-93 lifecycle state when");
    eprintln!("                               recorded (exited / in-scope).");
    eprintln!("  nmbrs metrics match  <expr>   Flat list of full");
    eprintln!("                               `family{{labels}}` specs that");
    eprintln!("                               match — copy-paste into other");
    eprintln!("                               commands or sanity-check a");
    eprintln!("                               filter pattern.");
    eprintln!("  nmbrs metrics groups <expr>   Pivot matching instances into");
    eprintln!("    --by k1,k2[,...]           a small table grouped by the");
    eprintln!("                               named label keys, with row");
    eprintln!("                               counts and value aggregates.");
    eprintln!("                               Use to answer questions like");
    eprintln!("                               \"are these two label values");
    eprintln!("                               actually distinct in the data?\"");
    eprintln!("  nmbrs metrics query  <expr>   Evaluate a metricsql");
    eprintln!("                               expression against the db.");
    eprintln!("                               Run `nmbrs metrics query` with");
    eprintln!("                               no args for full flag list.");
    eprintln!("  nmbrs metrics watch  <expr>   Live-update a metricsql");
    eprintln!("                               expression on a polling");
    eprintln!("                               interval. Uses the streaming");
    eprintln!("                               engine when supported, batch");
    eprintln!("                               eval otherwise.");
    eprintln!();
    eprintln!("Filter expressions:");
    eprintln!("  recall*                      Family-name glob.");
    eprintln!("  recall@10.mean{{k=\"10\"}}      OpenMetrics-style label match.");
    eprintln!("  {{profile=~label}}             Substring filter.");
    eprintln!();
    eprintln!("Source selection:");
    eprintln!("  --db <path>                  Override metrics db.");
    eprintln!("                               Default: logs/latest/metrics.db");
    eprintln!("  --session <path-or-name>     SRD-04 umbrella; redirects");
    eprintln!("                               logs/latest before reading.");
    eprintln!();
    eprintln!("Output:");
    eprintln!("  --format <name>              plain | json | jsonl | yaml | csv");
    eprintln!("                               | metricsql");
    eprintln!("                               Default: plain. `metricsql` emits");
    eprintln!("                               one selector line per instance —");
    eprintln!("                               family{{key=\"value\",…}} — with");
    eprintln!("                               labels in natural alphanumeric");
    eprintln!("                               order so every line shares the");
    eprintln!("                               same key sequence (copy-paste");
    eprintln!("                               into match / query tools).");
    eprintln!("  --list                       Names only — omit the per-leaf");
    eprintln!("                               value summary (families + label");
    eprintln!("                               dimensions, no values).");
    eprintln!("  --tofile <path>              Write to <path>. With no");
    eprintln!("                               --format, the extension chooses");
    eprintln!("                               (.json, .jsonl, .yaml, .csv,");
    eprintln!("                               .metricsql → metricsql,");
    eprintln!("                               .txt → plain).");
    eprintln!("  --tree                       Reshape json/yaml output into a");
    eprintln!("                               nested map keyed family →");
    eprintln!("                               constants + dim tree. Roll up by");
    eprintln!("                               family with `jq '.families.<n>'`.");
    eprintln!("                               Implies --format yaml when used");
    eprintln!("                               alone; rejects csv / jsonl /");
    eprintln!("                               metricsql.");
}

/// SRD-77 — shared parser fragment for the
/// `--execution=<n>` / `--all-executions` flag pair that
/// every `nmbrs metrics` subcommand accepts. Returns `true`
/// when the arg was consumed, so the caller's match arm can
/// dispatch with `if exec_flag_consumed(&mut state, arg, iter)
/// { continue; }`. Stores into a unified `MetricsExecFlag`
/// state — `Latest` (the default) becomes `Some(max_id)` at
/// resolution time, `Specific(n)` narrows to one execution,
/// `All` aggregates.
fn metrics_exec_flag_consumed<'a, I>(state: &mut MetricsExecFlag, arg: &str, iter: &mut I) -> bool
where
    I: Iterator<Item = &'a String>,
{
    match arg {
        "--all-executions" => {
            *state = MetricsExecFlag::All;
            true
        }
        "--execution" => {
            if let Some(v) = iter.next() {
                match v.parse::<u64>() {
                    Ok(n) => {
                        *state = MetricsExecFlag::Specific(n);
                        true
                    }
                    Err(_) => {
                        eprintln!("nmbrs metrics: --execution requires an integer, got '{v}'");
                        std::process::exit(2);
                    }
                }
            } else {
                eprintln!("nmbrs metrics: --execution requires a value");
                std::process::exit(2);
            }
        }
        other if other.starts_with("--execution=") => {
            let v = &other[12..];
            match v.parse::<u64>() {
                Ok(n) => {
                    *state = MetricsExecFlag::Specific(n);
                    true
                }
                Err(_) => {
                    eprintln!("nmbrs metrics: --execution requires an integer, got '{v}'");
                    std::process::exit(2);
                }
            }
        }
        _ => false,
    }
}

/// SRD-77 — operator-facing execution selection. `Latest` is
/// the default (implicit "show me the latest execution" with a
/// banner when more than one exists); `Specific(n)` and `All`
/// are explicit opt-ins via the CLI flags above.
#[derive(Debug, Clone, Copy, Default)]
enum MetricsExecFlag {
    #[default]
    Latest,
    Specific(u64),
    All,
}

impl MetricsExecFlag {
    /// Resolve this operator-facing flag to a concrete
    /// storage-layer `exec_id_filter`. Latest → emit the
    /// multi-exec banner (silent for single-exec sessions) and
    /// pick `max(exec_id)`. Specific → that exec id. All →
    /// `None` (the explicit aggregate-across-executions
    /// intent, which translates to no `WHERE exec_id = …`
    /// predicate at the storage boundary).
    fn resolve(self, db_path: &Path) -> Option<u64> {
        let session_dir = db_path.parent().map(Path::to_path_buf).unwrap_or_default();
        match self {
            MetricsExecFlag::Latest => {
                nmbrs_runtime::refine_plan::warn_multi_execution_default(&session_dir);
                nmbrs_runtime::refine_plan::ExecutionQualifier::latest(&session_dir).specific_id()
            }
            MetricsExecFlag::Specific(n) => Some(n),
            MetricsExecFlag::All => None,
        }
    }
}

/// `nmbrs metrics match <expr>` — print a flat list of every
/// `family{labels}` spec that matches the filter, one per line.
/// Unlike `list`/`show` (which group hierarchically by label
/// dimension), `match` preserves the spec verbatim so the
/// output round-trips into other commands that take a fully
/// qualified metric instance reference.
/// The metrics db these read commands should open: an explicit `--db`, else the
/// session named on the line (`--session*`, or a bare `session=`), else the
/// latest session.
///
/// The middle step is the one that was missing. All three parsers below SWALLOWED
/// `--session*` and its value and then fell back to the latest session, on the
/// premise that the umbrella applies it first — and the umbrella does so by
/// repointing the `sessions/latest` symlink, which only works for a session under
/// `sessions/` and makes a read command mutate global state. Resolving locally,
/// the way `report` and `summary` do, works for any path and mutates nothing.
fn resolve_db(db_flag: Option<PathBuf>, args: &[String]) -> PathBuf {
    db_flag
        .or_else(|| nmbrs_runtime::session::read_session_dir(args).map(|d| d.join("metrics.db")))
        .unwrap_or_else(nmbrs_runtime::session::latest_metrics_db)
}

/// SRD-93 A4 — every `nmbrs metrics` subcommand is a read view, so the
/// db opens READ-ONLY: an RO handle cannot perturb a live session's
/// WAL, and a typo'd write anywhere downstream fails instead of
/// mutating the session.
fn open_metrics_db_ro(db: &Path) -> Result<rusqlite::Connection, rusqlite::Error> {
    rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
}

/// SRD-93 stage 5 / M6 — `nmbrs metrics last [<expr>]`: the last
/// recorded sample per matching instance. One indexed
/// `ORDER BY timestamp_ms DESC LIMIT 1` per instance on a
/// consolidated db (user_version ≥ 4); on a live unindexed db each
/// lookup is a linear scan, announced once on stderr so the cost is
/// never a surprise. Lifecycle context (exited / in-scope) rides
/// along when the scope-event table exists.
fn last_specs(args: &[String]) {
    let mut db_path: Option<PathBuf> = None;
    let mut filter_expr: Option<String> = None;
    let mut exec_flag = MetricsExecFlag::default();
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if metrics_exec_flag_consumed(&mut exec_flag, a, &mut iter) {
            continue;
        }
        match a.as_str() {
            "--db" => {
                db_path = iter.next().map(PathBuf::from);
            }
            other if other.starts_with("--db=") => {
                db_path = Some(PathBuf::from(&other[5..]));
            }
            "--session"
            | "--session-name"
            | "--session-path"
            | "--session-reuse"
            | "--session-keep"
            | "--session-shelflife" => {
                let _ = iter.next();
            }
            other if other.starts_with("--session") => {}
            other if !other.starts_with("--") => {
                filter_expr = Some(other.to_string());
            }
            other => {
                eprintln!("nmbrs metrics last: unknown flag '{other}'");
                std::process::exit(2);
            }
        }
    }
    let filter = filter_expr.as_deref().map(parse_filter).transpose();
    let filter = match filter {
        Ok(f) => f,
        Err(e) => {
            eprintln!("nmbrs metrics last: filter: {e}");
            std::process::exit(2);
        }
    };
    let db = resolve_db(db_path, args);
    if !db.exists() {
        eprintln!("nmbrs metrics last: db not found at '{}'", db.display());
        std::process::exit(2);
    }
    let conn = match open_metrics_db_ro(&db) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nmbrs metrics last: open '{}': {e}", db.display());
            std::process::exit(2);
        }
    };
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);
    if version < 4 {
        eprintln!(
            "nmbrs metrics last: db is unindexed (live or crashed \
             session) — each lookup is a linear scan (~0.2s per \
             instance at 5M rows)."
        );
    }
    let exec_filter = exec_flag.resolve(&db);
    let (sql, params): (&str, Vec<i64>) = match exec_filter {
        Some(id) => (
            "SELECT id, spec FROM metric_instance WHERE exec_id = ?1 ORDER BY spec",
            vec![id as i64],
        ),
        None => (
            "SELECT id, spec FROM metric_instance ORDER BY spec",
            Vec::new(),
        ),
    };
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("nmbrs metrics last: query: {e}");
            std::process::exit(2);
        }
    };
    let rows: Vec<(i64, String)> = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default();
    let scopes = load_scope_annotations(&conn);
    let mut shown = 0usize;
    for (id, spec) in &rows {
        let (family, labels) = split_spec(spec);
        if let Some(f) = filter.as_ref()
            && !f.matches(&family, &labels)
        {
            continue;
        }
        shown += 1;
        #[allow(clippy::type_complexity)]
        let row: Result<(i64, Option<i64>, Option<f64>, Option<f64>, Option<f64>), _> = conn
            .query_row(
                "SELECT timestamp_ms, count, mean, p50, p99 \
             FROM sample_value WHERE instance_id = ?1 \
             ORDER BY timestamp_ms DESC LIMIT 1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            );
        let scope = scopes
            .get(id)
            .map(|a| format!("  [{a}]"))
            .unwrap_or_default();
        match row {
            Ok((ts, count, mean, p50, p99)) => {
                let mut parts: Vec<String> = Vec::new();
                if let Some(c) = count {
                    parts.push(format!("count={c}"));
                }
                if let Some(m) = mean {
                    parts.push(format!("mean={m:.4}"));
                }
                if let Some(p) = p50 {
                    parts.push(format!("p50={p:.4}"));
                }
                if let Some(p) = p99 {
                    parts.push(format!("p99={p:.4}"));
                }
                println!("{spec}  @{ts}ms {}{scope}", parts.join(" "));
            }
            Err(_) => println!("{spec}  (no samples){scope}"),
        }
    }
    if shown == 0 {
        match filter_expr {
            Some(ref e) => println!("(no metrics matching '{e}')"),
            None => println!("(no metrics)"),
        }
    }
}

fn match_specs(args: &[String]) {
    let mut db_path: Option<PathBuf> = None;
    let mut filter_expr: Option<String> = None;
    let mut exec_flag = MetricsExecFlag::default();
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if metrics_exec_flag_consumed(&mut exec_flag, a, &mut iter) {
            continue;
        }
        match a.as_str() {
            "--db" => {
                db_path = iter.next().map(PathBuf::from);
            }
            other if other.starts_with("--db=") => {
                db_path = Some(PathBuf::from(&other[5..]));
            }
            "--session"
            | "--session-name"
            | "--session-path"
            | "--session-reuse"
            | "--session-keep"
            | "--session-shelflife" => {
                let _ = iter.next();
            }
            other if other.starts_with("--session") => {}
            other if !other.starts_with("--") => {
                filter_expr = Some(other.to_string());
            }
            other => {
                eprintln!("nmbrs metrics match: unknown flag '{other}'");
                std::process::exit(2);
            }
        }
    }
    let Some(expr) = filter_expr else {
        eprintln!("nmbrs metrics match: pattern required");
        eprintln!("  e.g. `nmbrs metrics match 'recall*'`");
        eprintln!("       `nmbrs metrics match 'recall@10.mean{{k=\"10\"}}'`");
        std::process::exit(2);
    };
    let filter = match parse_filter(&expr) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("nmbrs metrics match: filter: {e}");
            std::process::exit(2);
        }
    };
    let db = resolve_db(db_path, args);
    if !db.exists() {
        eprintln!("nmbrs metrics match: db not found at '{}'", db.display());
        std::process::exit(2);
    }
    let conn = match open_metrics_db_ro(&db) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nmbrs metrics match: open '{}': {e}", db.display());
            std::process::exit(2);
        }
    };
    // SRD-77 — push the execution qualifier into the SQL
    // boundary. The default flag (`Latest`) emits the
    // multi-exec banner here; the operator can override via
    // `--execution=<n>` / `--all-executions`.
    let exec_filter = exec_flag.resolve(&db);
    let (sql, params): (&str, Vec<i64>) = match exec_filter {
        Some(id) => (
            "SELECT spec FROM metric_instance WHERE exec_id = ?1 ORDER BY spec",
            vec![id as i64],
        ),
        None => ("SELECT spec FROM metric_instance ORDER BY spec", Vec::new()),
    };
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("nmbrs metrics match: query: {e}");
            std::process::exit(2);
        }
    };
    let rows: Vec<String> = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |r| {
            r.get::<_, String>(0)
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default();

    let mut count = 0;
    for spec in &rows {
        let (family, labels) = split_spec(spec);
        if filter.matches(&family, &labels) {
            println!("{spec}");
            count += 1;
        }
    }
    eprintln!(
        "# {} match{} ({} total instances)",
        count,
        if count == 1 { "" } else { "es" },
        rows.len(),
    );
}

/// Running aggregate accumulated by `groups_command` for
/// each distinct label tuple — `BTreeMap` keyed on the
/// per-row tuple of requested label values, value is the
/// per-group totals.
struct GroupBucket {
    instances: usize,
    rows: i64,
    sum: f64,
    min: f64,
    max: f64,
}

impl GroupBucket {
    fn new() -> Self {
        Self {
            instances: 0,
            rows: 0,
            sum: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }
}

/// Trait-shaped read for a group bucket — lets the
/// `render_groups_*` helpers stay generic without
/// exposing the private struct shape directly to every
/// caller.
trait GroupBucketRead {
    fn instances(&self) -> usize;
    fn rows(&self) -> i64;
    fn sum(&self) -> f64;
    fn min(&self) -> f64;
    fn max(&self) -> f64;
}

impl GroupBucketRead for GroupBucket {
    fn instances(&self) -> usize {
        self.instances
    }
    fn rows(&self) -> i64 {
        self.rows
    }
    fn sum(&self) -> f64 {
        self.sum
    }
    fn min(&self) -> f64 {
        self.min
    }
    fn max(&self) -> f64 {
        self.max
    }
}

/// `nmbrs metrics groups <expr> --by k1,k2[,...]` — pivot
/// every matching metric instance's samples into a small
/// table keyed by the requested label tuple. Answers the
/// question "for these instances, broken out by these
/// labels, how many rows are there and what's the
/// aggregate?"
///
/// Replaces the SQL-three-way-join the user otherwise has
/// to write against `sample_value` / `metric_instance` /
/// `label_set_entry`. The filter expression syntax is the
/// same one `nmbrs metrics list` / `show` / `match` already
/// accept (`family{label="value"}`).
///
/// Example:
///
/// ```text
/// nmbrs metrics groups 'recall_mean{phase="ann_query"}' --by optimize_for,k
/// ```
///
/// Output:
///
/// ```text
/// optimize_for | k   | instances | rows | mean   | min  | max
/// LATENCY      | 1   |        12 | 4290 | 0.7864 | 0.00 | 1.00
/// LATENCY      | 10  |        12 | 4288 | 0.9134 | 0.20 | 1.00
/// RECALL       | 1   |        12 | 4290 | 0.7860 | 0.00 | 1.00
/// RECALL       | 10  |        12 | 4288 | 0.9128 | 0.20 | 1.00
/// ```
///
/// Use `(none)` as the displayed value for instances that
/// don't carry the requested label key — the row is still
/// shown so the operator notices missing-label bugs.
fn groups_command(args: &[String]) {
    let mut db_path: Option<PathBuf> = None;
    let mut filter_expr: Option<String> = None;
    let mut by_keys: Vec<String> = Vec::new();
    let mut format = Format::Plain;
    let mut exec_flag = MetricsExecFlag::default();
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if metrics_exec_flag_consumed(&mut exec_flag, a, &mut iter) {
            continue;
        }
        match a.as_str() {
            "--db" => {
                db_path = iter.next().map(PathBuf::from);
            }
            other if other.starts_with("--db=") => {
                db_path = Some(PathBuf::from(&other[5..]));
            }
            "--by" => {
                by_keys = iter
                    .next()
                    .map(|s| {
                        s.split(',')
                            .map(|k| k.trim().to_string())
                            .filter(|k| !k.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
            }
            other if other.starts_with("--by=") => {
                by_keys = other[5..]
                    .split(',')
                    .map(|k| k.trim().to_string())
                    .filter(|k| !k.is_empty())
                    .collect();
            }
            "--format" => {
                if let Some(name) = iter.next() {
                    match Format::parse(name) {
                        Ok(f) => format = f,
                        Err(e) => {
                            eprintln!("nmbrs metrics groups: {e}");
                            std::process::exit(2);
                        }
                    }
                }
            }
            other if other.starts_with("--format=") => match Format::parse(&other[9..]) {
                Ok(f) => format = f,
                Err(e) => {
                    eprintln!("nmbrs metrics groups: {e}");
                    std::process::exit(2);
                }
            },
            "--session"
            | "--session-name"
            | "--session-path"
            | "--session-reuse"
            | "--session-keep"
            | "--session-shelflife" => {
                let _ = iter.next();
            }
            other if other.starts_with("--session") => {}
            other if !other.starts_with("--") => {
                filter_expr = Some(other.to_string());
            }
            other => {
                eprintln!("nmbrs metrics groups: unknown flag '{other}'");
                std::process::exit(2);
            }
        }
    }

    let Some(expr) = filter_expr else {
        eprintln!("nmbrs metrics groups: filter expression required");
        eprintln!("  e.g. `nmbrs metrics groups 'recall_mean{{phase=\"ann_query\"}}' \\");
        eprintln!("        --by optimize_for,k`");
        std::process::exit(2);
    };
    if by_keys.is_empty() {
        eprintln!("nmbrs metrics groups: --by <key,...> required");
        eprintln!("  Specify which label keys to pivot on, e.g. `--by optimize_for,k`.");
        std::process::exit(2);
    }
    let filter = match parse_filter(&expr) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("nmbrs metrics groups: filter: {e}");
            std::process::exit(2);
        }
    };

    let db = resolve_db(db_path, args);
    if !db.exists() {
        eprintln!("nmbrs metrics groups: db not found at '{}'", db.display());
        std::process::exit(2);
    }
    let conn = match open_metrics_db_ro(&db) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nmbrs metrics groups: open '{}': {e}", db.display());
            std::process::exit(2);
        }
    };

    // Walk every metric_instance, filter, accumulate per-
    // group aggregates. We aggregate at the
    // `sample_value`-row level (every row counts as one
    // observation), not at the instance level — so a phase
    // that ran 1000 samples weighs 1000× a phase with one
    // sample. That's the right shape for the recall /
    // latency questions this is designed to answer.
    // SRD-77 — push the execution qualifier into the
    // metric_instance SELECT so the aggregation only sees
    // instances from the qualified execution(s).
    let exec_filter = exec_flag.resolve(&db);
    let (instances_sql, instances_params): (&str, Vec<i64>) = match exec_filter {
        Some(id) => (
            "SELECT mi.id, mi.spec FROM metric_instance mi WHERE mi.exec_id = ?1",
            vec![id as i64],
        ),
        None => ("SELECT mi.id, mi.spec FROM metric_instance mi", Vec::new()),
    };
    let mut stmt = match conn.prepare(instances_sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("nmbrs metrics groups: query: {e}");
            std::process::exit(2);
        }
    };
    let instances: Vec<(i64, String)> = stmt
        .query_map(rusqlite::params_from_iter(instances_params.iter()), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default();

    let mut buckets: BTreeMap<Vec<String>, GroupBucket> = BTreeMap::new();

    for (id, spec) in &instances {
        let (family, labels) = split_spec(spec);
        if !filter.matches(&family, &labels) {
            continue;
        }
        // Look up each requested key on this instance.
        let key_tuple: Vec<String> = by_keys
            .iter()
            .map(|k| {
                labels
                    .iter()
                    .find(|(kk, _)| kk == k)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_else(|| "(none)".to_string())
            })
            .collect();

        // Aggregate every sample row. count is the per-sample
        // observation count (a histogram bucket can carry many
        // observations in one sample row); fall back to 1 for
        // gauges that only have a `mean` value.
        let mut sql = match conn.prepare(
            "SELECT COALESCE(count, 1), \
                    mean, \
                    min, \
                    max \
             FROM sample_value WHERE instance_id = ?1",
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("nmbrs metrics groups: prepare: {e}");
                std::process::exit(2);
            }
        };
        let bucket = buckets.entry(key_tuple).or_insert_with(GroupBucket::new);
        bucket.instances += 1;
        let rows_iter = sql.query_map([id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<f64>>(1)?,
                r.get::<_, Option<f64>>(2)?,
                r.get::<_, Option<f64>>(3)?,
            ))
        });
        if let Ok(rows_iter) = rows_iter {
            for row in rows_iter.flatten() {
                let (cnt, mean, mn, mx) = row;
                bucket.rows += cnt;
                if let Some(m) = mean {
                    bucket.sum += m * cnt as f64;
                }
                if let Some(v) = mn
                    && v < bucket.min
                {
                    bucket.min = v;
                }
                if let Some(v) = mx
                    && v > bucket.max
                {
                    bucket.max = v;
                }
            }
        }
    }

    if buckets.is_empty() {
        eprintln!(
            "nmbrs metrics groups: no matching metric instances. \
             Try `nmbrs metrics match '{expr}'` to see what's there."
        );
        std::process::exit(1);
    }

    // Render. CSV / JSON / plain are the three shapes the
    // existing `list` surface speaks; reuse them so the
    // output round-trips through any pipeline that already
    // consumes those.
    match format {
        Format::Csv => render_groups_csv(&by_keys, &buckets),
        Format::Json => render_groups_json(&by_keys, &buckets),
        _ => render_groups_plain(&by_keys, &buckets),
    }
}

fn render_groups_plain(by_keys: &[String], buckets: &BTreeMap<Vec<String>, impl GroupBucketRead>) {
    // Width-align so the table reads as a tidy column dump.
    let mut headers: Vec<String> = by_keys.to_vec();
    headers.extend(
        ["instances", "rows", "mean", "min", "max"]
            .iter()
            .map(|s| s.to_string()),
    );

    /// Format an aggregate value, replacing the f64
    /// sentinels we use for "no observations" with a
    /// dash. Without this, gauges that don't carry
    /// per-sample min/max columns render as `inf` /
    /// `-inf` — meaningless to the operator.
    fn fmt_agg(v: f64) -> String {
        if v.is_finite() {
            format!("{v:.4}")
        } else {
            "-".to_string()
        }
    }

    let mut grid: Vec<Vec<String>> = Vec::new();
    grid.push(headers.clone());
    for (key, b) in buckets {
        let mean = if b.rows() > 0 {
            b.sum() / b.rows() as f64
        } else {
            f64::NAN
        };
        let mut row: Vec<String> = key.clone();
        row.push(b.instances().to_string());
        row.push(b.rows().to_string());
        row.push(fmt_agg(mean));
        row.push(fmt_agg(b.min()));
        row.push(fmt_agg(b.max()));
        grid.push(row);
    }
    let widths: Vec<usize> = (0..grid[0].len())
        .map(|c| grid.iter().map(|row| row[c].len()).max().unwrap_or(0))
        .collect();
    for (i, row) in grid.iter().enumerate() {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(c, v)| format!("{v:width$}", width = widths[c]))
            .collect();
        println!("{}", line.join(" | "));
        if i == 0 {
            // Underline header.
            let underline: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
            println!("{}", underline.join("-+-"));
        }
    }
}

fn render_groups_csv(by_keys: &[String], buckets: &BTreeMap<Vec<String>, impl GroupBucketRead>) {
    let mut header: Vec<&str> = by_keys.iter().map(String::as_str).collect();
    header.extend(["instances", "rows", "mean", "min", "max"]);
    println!("{}", header.join(","));
    for (key, b) in buckets {
        let mean = if b.rows() > 0 {
            b.sum() / b.rows() as f64
        } else {
            f64::NAN
        };
        let mut row: Vec<String> = key.iter().map(|v| csv_escape(v)).collect();
        row.push(b.instances().to_string());
        row.push(b.rows().to_string());
        row.push(format!("{mean:.6}"));
        row.push(format!("{:.6}", b.min()));
        row.push(format!("{:.6}", b.max()));
        println!("{}", row.join(","));
    }
}

fn render_groups_json(by_keys: &[String], buckets: &BTreeMap<Vec<String>, impl GroupBucketRead>) {
    let rows: Vec<serde_json::Value> = buckets
        .iter()
        .map(|(key, b)| {
            let mut obj = serde_json::Map::new();
            for (i, k) in by_keys.iter().enumerate() {
                obj.insert(k.clone(), serde_json::Value::String(key[i].clone()));
            }
            let mean = if b.rows() > 0 {
                b.sum() / b.rows() as f64
            } else {
                f64::NAN
            };
            obj.insert(
                "instances".into(),
                serde_json::Value::Number(serde_json::Number::from(b.instances() as i64)),
            );
            obj.insert(
                "rows".into(),
                serde_json::Value::Number(serde_json::Number::from(b.rows())),
            );
            if mean.is_finite() {
                obj.insert(
                    "mean".into(),
                    serde_json::Value::Number(
                        serde_json::Number::from_f64(mean).unwrap_or(serde_json::Number::from(0)),
                    ),
                );
            }
            if b.min().is_finite() {
                obj.insert(
                    "min".into(),
                    serde_json::Value::Number(
                        serde_json::Number::from_f64(b.min())
                            .unwrap_or(serde_json::Number::from(0)),
                    ),
                );
            }
            if b.max().is_finite() {
                obj.insert(
                    "max".into(),
                    serde_json::Value::Number(
                        serde_json::Number::from_f64(b.max())
                            .unwrap_or(serde_json::Number::from(0)),
                    ),
                );
            }
            serde_json::Value::Object(obj)
        })
        .collect();
    let out = serde_json::Value::Array(rows);
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
}

/// Chosen serialization for `nmbrs metrics list` / `show`.
/// `Plain` is the hierarchical tree view; everything else is
/// machine-readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Plain,
    Json,
    Jsonl,
    Yaml,
    Csv,
    /// MetricsQL / OpenMetrics canonical line form:
    /// `family{key="value",key="value",…}` — one instance per
    /// line, label keys sorted in natural alphanumeric order so
    /// every line shares the same key sequence and pivots /
    /// diffs / line-by-line readers see consistent column
    /// alignment. Output is directly copy-pasteable into
    /// `nmbrs metrics match` / `nmbrs metrics query` /
    /// downstream PromQL-compatible tooling.
    MetricsQL,
}

impl Format {
    fn parse(name: &str) -> Result<Self, String> {
        match name.to_ascii_lowercase().as_str() {
            "plain" | "txt" | "text" | "tree" => Ok(Format::Plain),
            "json" => Ok(Format::Json),
            "jsonl" | "ndjson" => Ok(Format::Jsonl),
            "yaml" | "yml" => Ok(Format::Yaml),
            "csv" => Ok(Format::Csv),
            "metricsql" => Ok(Format::MetricsQL),
            other => Err(format!(
                "unknown format '{other}' (expected one of: \
                 plain, json, jsonl, yaml, csv, metricsql)"
            )),
        }
    }

    /// Pick a format from a file extension. `.txt` (and unknown
    /// extensions) map to `Plain`.
    fn from_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "json" => Format::Json,
            "jsonl" | "ndjson" => Format::Jsonl,
            "yaml" | "yml" => Format::Yaml,
            "csv" => Format::Csv,
            "metricsql" => Format::MetricsQL,
            _ => Format::Plain,
        }
    }
}

/// Quote a label value for MetricsQL / OpenMetrics output.
/// Escapes per the OpenMetrics spec: `\` → `\\`, `"` → `\"`,
/// newline → `\n`. Bare other-bytes pass through.
fn escape_metricsql_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('"');
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Render flat instance rows as one OpenMetrics-canonical line
/// per instance: `family{key="value",key="value",…}`. Labels
/// within each line are sorted by [`cmp_natural`] (the
/// existing natural-alphanumeric comparator used elsewhere in
/// this module) so all lines share the same key sequence and
/// readers can rely on columnar alignment.
fn render_metricsql(w: &mut dyn std::io::Write, flat: &[InstanceRow]) -> std::io::Result<()> {
    for row in flat {
        let mut labels: Vec<&(String, String)> = row.labels.iter().collect();
        labels.sort_by(|a, b| cmp_natural(&a.0, &b.0));
        write!(w, "{}", row.family)?;
        if !labels.is_empty() {
            write!(w, "{{")?;
            let mut first = true;
            for (k, v) in &labels {
                if !first {
                    write!(w, ",")?;
                }
                first = false;
                write!(w, "{}={}", k, escape_metricsql_value(v))?;
            }
            write!(w, "}}")?;
        }
        writeln!(w)?;
    }
    Ok(())
}

/// One row in the structured output. `values` is populated only
/// when `show_values` is set (i.e., the user asked for `nmbrs
/// metrics show`).
#[derive(Debug, Clone)]
struct InstanceRow {
    family: String,
    /// Labels in declaration order from the spec — preserves
    /// the same key sequence the producer used. The structured
    /// formats serialize this as an ordered map.
    labels: Vec<(String, String)>,
    values: Option<ValueSummary>,
}

/// What the leading count figure on a leaf means, so the
/// rendered word matches the metric kind instead of always
/// claiming "samples". A counter's headline is its cumulative
/// total; a summary's is its observation count; a gauge's is the
/// number of readings.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
enum CountKind {
    /// Number of sampled readings (gauges and other scalar series).
    #[default]
    Samples,
    /// Cumulative counter total.
    Total,
    /// Cumulative observation count behind a distribution summary.
    Obs,
}

impl CountKind {
    fn word(self) -> &'static str {
        match self {
            CountKind::Samples => "samples",
            CountKind::Total => "total",
            CountKind::Obs => "obs",
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ValueSummary {
    count: Option<i64>,
    /// Interpretation of [`count`](Self::count) for this leaf —
    /// drives the rendered word and reflects the metric kind.
    count_kind: CountKind,
    mean: Option<f64>,
    p50: Option<f64>,
    p99: Option<f64>,
    min: Option<f64>,
    max: Option<f64>,
    stddev: Option<f64>,
    /// Earliest sample_value.timestamp_ms for this instance.
    /// Combined with [`ts_max_ms`] gives the span the summary
    /// covers.
    ts_min_ms: Option<i64>,
    ts_max_ms: Option<i64>,
}

fn list(args: &[String], show_values_in: bool) {
    let mut db_path: Option<PathBuf> = None;
    let mut filter_expr: Option<String> = None;
    let mut format_arg: Option<String> = None;
    let mut tofile: Option<PathBuf> = None;
    let mut tree_mode: bool = false;
    // `--list` renders names only (families + label dimensions, no value
    // summary) — the consolidated former `list` subcommand. It is applied
    // AFTER parsing so it wins over `--tree`'s value promotion and the
    // `show` default.
    let mut list_mode: bool = false;
    // SRD-93 stage 5 — `--scope` annotates plain-format leaves with
    // the instance's lifecycle state from `instance_scope_event`.
    let mut scope_mode: bool = false;
    // `show_values` may be promoted by `--tree`: tree mode's
    // leaves *are* the value summary — null leaves carry no
    // information, so loading the summary is required for the
    // output to be useful.
    let mut show_values: bool = show_values_in;
    let mut exec_flag = MetricsExecFlag::default();
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if metrics_exec_flag_consumed(&mut exec_flag, a, &mut iter) {
            continue;
        }
        match a.as_str() {
            "--db" => {
                db_path = iter.next().map(PathBuf::from);
            }
            other if other.starts_with("--db=") => {
                db_path = Some(PathBuf::from(&other[5..]));
            }
            "--format" => {
                format_arg = iter.next().cloned();
            }
            other if other.starts_with("--format=") => {
                format_arg = Some(other[9..].to_string());
            }
            "--tofile" | "--to-file" => {
                tofile = iter.next().map(PathBuf::from);
            }
            other if other.starts_with("--tofile=") => {
                tofile = Some(PathBuf::from(&other[9..]));
            }
            other if other.starts_with("--to-file=") => {
                tofile = Some(PathBuf::from(&other[10..]));
            }
            "--tree" => {
                tree_mode = true;
                show_values = true;
            }
            "--list" => {
                list_mode = true;
            }
            "--scope" => {
                scope_mode = true;
            }
            // Globals consumed at startup.
            "--session"
            | "--session-name"
            | "--session-path"
            | "--session-reuse"
            | "--session-keep"
            | "--session-shelflife" => {
                let _ = iter.next();
            }
            other if other.starts_with("--session") => {}
            other if !other.starts_with("--") => {
                filter_expr = Some(other.to_string());
            }
            other => {
                eprintln!("nmbrs metrics: unknown flag '{other}'");
                std::process::exit(2);
            }
        }
    }
    // `--list` (names only) wins over both the `show` default and
    // `--tree`'s value promotion: a list view never carries per-leaf
    // values, whatever the layout.
    if list_mode {
        show_values = false;
    }

    // Resolve format. Explicit `--format` wins; otherwise derive
    // from the `--tofile` extension; otherwise default to plain
    // — except `--tree` alone implies yaml (since plain already
    // is a tree view).
    let format = match (format_arg.as_deref(), tofile.as_deref()) {
        (Some(name), _) => match Format::parse(name) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("nmbrs metrics: {e}");
                std::process::exit(2);
            }
        },
        (None, Some(path)) => path
            .extension()
            .and_then(|s| s.to_str())
            .map(Format::from_extension)
            .unwrap_or(Format::Plain),
        (None, None) if tree_mode => Format::Yaml,
        (None, None) => Format::Plain,
    };

    // `--tree` reshapes structured output into a hierarchical
    // map keyed by family → label-key → label-value → … Only
    // json/yaml carry that structure; csv is by definition
    // tabular, jsonl is line-delimited, plain is already a
    // tree (its own renderer). Reject the combinations that
    // can't carry hierarchy rather than silently falling back.
    if tree_mode && !matches!(format, Format::Json | Format::Yaml) {
        eprintln!(
            "nmbrs metrics: --tree requires --format json or yaml \
             (csv/jsonl/metricsql are flat; plain is already a tree)"
        );
        std::process::exit(2);
    }

    let db = resolve_db(db_path, args);
    if !db.exists() {
        eprintln!("nmbrs metrics: db not found at '{}'", db.display());
        std::process::exit(2);
    }
    let conn = match open_metrics_db_ro(&db) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nmbrs metrics: open '{}': {e}", db.display());
            std::process::exit(2);
        }
    };
    let filter = filter_expr.as_deref().map(parse_filter).transpose();
    let filter = match filter {
        Ok(f) => f,
        Err(e) => {
            eprintln!("nmbrs metrics: filter: {e}");
            std::process::exit(2);
        }
    };
    // SRD-77 — qualify the instance listing by the operator's
    // execution selector. Default is Latest with multi-exec
    // banner; `--execution=<n>` narrows; `--all-executions`
    // pulls across every recorded execution.
    let exec_filter = exec_flag.resolve(&db);
    let (sql, params): (&str, Vec<i64>) = match exec_filter {
        Some(id) => (
            "SELECT mi.id, mi.spec FROM metric_instance mi \
             WHERE mi.exec_id = ?1 ORDER BY mi.spec",
            vec![id as i64],
        ),
        None => (
            "SELECT mi.id, mi.spec FROM metric_instance mi ORDER BY mi.spec",
            Vec::new(),
        ),
    };
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("nmbrs metrics: query: {e}");
            std::process::exit(2);
        }
    };
    let rows: Vec<(i64, String)> = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default();

    // Bucket by family then by sorted label tuple. Used by the
    // tree renderer; structured renderers walk the flat list.
    let mut tree: BTreeMap<String, BTreeMap<Vec<(String, String)>, i64>> = BTreeMap::new();
    let mut kept: Vec<(i64, String, Vec<(String, String)>)> = Vec::new();
    for (id, spec) in &rows {
        let (family, labels) = split_spec(spec);
        if let Some(f) = filter.as_ref()
            && !f.matches(&family, &labels)
        {
            continue;
        }
        let mut sorted = labels.clone();
        sorted.sort();
        tree.entry(family.clone()).or_default().insert(sorted, *id);
        kept.push((*id, family, labels));
    }

    // SRD-93 stage 1 — value summaries are computed ONCE, by a single
    // ordered pass over `sample_value`, and shared by every renderer
    // (the plain tree renderer used to recompute each leaf with its
    // own per-instance query storm).
    let summaries: std::collections::HashMap<i64, ValueSummary> = if show_values {
        let ids: std::collections::HashSet<i64> = kept.iter().map(|(id, _, _)| *id).collect();
        load_all_value_summaries(&conn, &ids)
    } else {
        Default::default()
    };

    // SRD-93 stage 5 — pre-rendered lifecycle annotations per
    // instance (empty map when `--scope` is off or the db predates
    // the scope-event table).
    let scopes: std::collections::HashMap<i64, String> = if scope_mode {
        load_scope_annotations(&conn)
    } else {
        Default::default()
    };

    let mut flat: Vec<InstanceRow> = kept
        .into_iter()
        .map(|(id, family, labels)| {
            let values = if show_values {
                Some(summaries.get(&id).cloned().unwrap_or_default())
            } else {
                None
            };
            InstanceRow {
                family,
                labels,
                values,
            }
        })
        .collect();

    // Natural-order sort the flat list: family by natural compare
    // (so `recall@2.mean` < `recall@10.mean`), then by label
    // values so two instances with `k="2"` / `k="10"` come out 2,
    // 10 — not lex 10, 2. The downstream structured renderers
    // (json/jsonl/yaml/csv) walk this Vec in order and inherit
    // the ordering as-is.
    flat.sort_by(|a, b| {
        cmp_natural(&a.family, &b.family)
            .then_with(|| cmp_label_pairs_natural(&a.labels, &b.labels))
    });

    if tree.is_empty() {
        let msg = match filter_expr {
            Some(ref e) => format!("(no metrics matching '{e}')"),
            None => "(no metrics)".to_string(),
        };
        // Empty result still goes through the chosen sink so
        // callers piping into a file get a valid empty document
        // rather than a stderr message they can't capture.
        match (format, tree_mode) {
            (Format::Plain, _) => emit(&tofile, format, |w| writeln!(w, "{msg}")),
            (Format::Json, false) => emit(&tofile, format, |w| {
                writeln!(w, "{}", render_json(&db, &flat, show_values))
            }),
            (Format::Json, true) => emit(&tofile, format, |w| {
                writeln!(w, "{}", render_tree_json(&db, &flat, show_values))
            }),
            (Format::Jsonl, _) => emit(&tofile, format, |_w| Ok(())),
            (Format::Yaml, false) => emit(&tofile, format, |w| {
                writeln!(w, "{}", render_yaml(&db, &flat, show_values))
            }),
            (Format::Yaml, true) => emit(&tofile, format, |w| {
                writeln!(w, "{}", render_tree_yaml(&db, &flat, show_values))
            }),
            (Format::Csv, _) => emit(&tofile, format, |w| {
                writeln!(w, "{}", render_csv_header(&flat, show_values))
            }),
            (Format::MetricsQL, _) => emit(&tofile, format, |_w| Ok(())),
        }
        return;
    }

    match (format, tree_mode) {
        (Format::Plain, _) => emit(&tofile, format, |w| {
            render_plain(w, &db, &tree, &rows, &summaries, &scopes, show_values)
        }),
        (Format::Json, false) => emit(&tofile, format, |w| {
            writeln!(w, "{}", render_json(&db, &flat, show_values))
        }),
        (Format::Json, true) => emit(&tofile, format, |w| {
            writeln!(w, "{}", render_tree_json(&db, &flat, show_values))
        }),
        (Format::Jsonl, _) => emit(&tofile, format, |w| {
            for row in &flat {
                writeln!(w, "{}", row_to_json(row, show_values))?;
            }
            Ok(())
        }),
        (Format::Yaml, false) => emit(&tofile, format, |w| {
            writeln!(w, "{}", render_yaml(&db, &flat, show_values))
        }),
        (Format::Yaml, true) => emit(&tofile, format, |w| {
            writeln!(w, "{}", render_tree_yaml(&db, &flat, show_values))
        }),
        (Format::Csv, _) => emit(&tofile, format, |w| render_csv(w, &flat, show_values)),
        (Format::MetricsQL, _) => emit(&tofile, format, |w| render_metricsql(w, &flat)),
    }
}

/// Run `f` against either the chosen `--tofile` sink or stdout.
/// I/O errors abort with a non-zero exit; the caller's closure
/// can return `io::Error` directly.
fn emit<F>(tofile: &Option<PathBuf>, _format: Format, f: F)
where
    F: FnOnce(&mut dyn std::io::Write) -> std::io::Result<()>,
{
    let result = match tofile {
        Some(path) => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::File::create(path) {
                Ok(file) => {
                    let mut bw = std::io::BufWriter::new(file);
                    let r = f(&mut bw);
                    r.and_then(|_| std::io::Write::flush(&mut bw))
                }
                Err(e) => {
                    eprintln!("nmbrs metrics: open '{}': {e}", path.display());
                    std::process::exit(2);
                }
            }
        }
        None => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            f(&mut lock)
        }
    };
    if let Err(e) = result {
        eprintln!("nmbrs metrics: write: {e}");
        std::process::exit(2);
    }
}

// The family → label-set → count tree is the natural shape of the
// grouped metric data here; aliasing it would not aid readability.
#[allow(clippy::type_complexity)]
fn render_plain(
    w: &mut dyn std::io::Write,
    db: &Path,
    tree: &BTreeMap<String, BTreeMap<Vec<(String, String)>, i64>>,
    rows: &[(i64, String)],
    summaries: &std::collections::HashMap<i64, ValueSummary>,
    scopes: &std::collections::HashMap<i64, String>,
    show_values: bool,
) -> std::io::Result<()> {
    writeln!(
        w,
        "# {} ({} famil{}, {} instance{})",
        db.display(),
        tree.len(),
        if tree.len() == 1 { "y" } else { "ies" },
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
    )?;
    // Natural-order family iteration — `recall@2.mean` before
    // `recall@10.mean`. Re-sort the BTreeMap key list rather
    // than swapping the bucket type (BTreeMap is doing the
    // grouping work, just not the final ordering).
    let mut family_names: Vec<&String> = tree.keys().collect();
    family_names.sort_by(|a, b| cmp_natural(a, b));
    for family in family_names {
        let instances = tree.get(family).unwrap();
        writeln!(w)?;
        // Collapse dimensions that have a single shared value
        // across every instance under this family — they're
        // not adding information to the tree, just noise.
        // Print them once at the family-level header.
        let label_sets: Vec<Vec<(String, String)>> = instances.keys().cloned().collect();
        let (constant_dims, varying_label_sets) = factor_constant_dims(&label_sets);
        if constant_dims.is_empty() {
            writeln!(w, "{family}")?;
        } else {
            let const_str = constant_dims
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(w, "{family}  [{const_str}]")?;
        }
        let varying_instances: BTreeMap<Vec<(String, String)>, i64> = instances
            .iter()
            .zip(varying_label_sets.iter())
            .map(|((_, id), labels)| (labels.clone(), *id))
            .collect();
        let dim_tree = build_dim_tree(varying_label_sets);
        write_dim_tree(
            w,
            &dim_tree,
            "  ",
            &varying_instances,
            summaries,
            scopes,
            show_values,
        )?;
    }
    Ok(())
}

fn row_to_json(row: &InstanceRow, show_values: bool) -> String {
    let labels: serde_json::Map<String, serde_json::Value> = row
        .labels
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    let mut obj = serde_json::Map::new();
    obj.insert(
        "family".into(),
        serde_json::Value::String(row.family.clone()),
    );
    obj.insert("labels".into(), serde_json::Value::Object(labels));
    if show_values {
        let v = row.values.clone().unwrap_or_default();
        let mut vobj = serde_json::Map::new();
        if let Some(c) = v.count {
            vobj.insert("count".into(), serde_json::json!(c));
        }
        if let Some(m) = v.mean {
            vobj.insert("mean".into(), serde_json::json!(m));
        }
        if let Some(m) = v.p50 {
            vobj.insert("p50".into(), serde_json::json!(m));
        }
        if let Some(m) = v.p99 {
            vobj.insert("p99".into(), serde_json::json!(m));
        }
        if let Some(m) = v.min {
            vobj.insert("min".into(), serde_json::json!(m));
        }
        if let Some(m) = v.max {
            vobj.insert("max".into(), serde_json::json!(m));
        }
        obj.insert("values".into(), serde_json::Value::Object(vobj));
    }
    serde_json::Value::Object(obj).to_string()
}

fn render_json(db: &Path, flat: &[InstanceRow], show_values: bool) -> String {
    let instances: Vec<serde_json::Value> = flat
        .iter()
        .map(|r| {
            serde_json::from_str(&row_to_json(r, show_values)).unwrap_or(serde_json::Value::Null)
        })
        .collect();
    let envelope = serde_json::json!({
        "db": db.display().to_string(),
        "count": flat.len(),
        "instances": instances,
    });
    serde_json::to_string_pretty(&envelope).unwrap_or_else(|_| "{}".to_string())
}

fn render_yaml(db: &Path, flat: &[InstanceRow], show_values: bool) -> String {
    let json = render_json(db, flat, show_values);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
    serde_yaml::to_string(&v).unwrap_or_default()
}

/// Build a tree-shaped json document keyed `family → constants +
/// nested label tree`. Lets the user roll up by family with
/// `jq '.families."recall@10.mean"'` (or the yaml equivalent)
/// and walk the dim tree structurally instead of by-row.
fn render_tree_json(db: &Path, flat: &[InstanceRow], show_values: bool) -> String {
    let value = build_tree_value(db, flat, show_values);
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string())
}

fn render_tree_yaml(db: &Path, flat: &[InstanceRow], show_values: bool) -> String {
    let value = build_tree_value(db, flat, show_values);
    serde_yaml::to_string(&value).unwrap_or_default()
}

fn build_tree_value(db: &Path, flat: &[InstanceRow], show_values: bool) -> serde_json::Value {
    let mut by_family: BTreeMap<String, Vec<&InstanceRow>> = BTreeMap::new();
    for row in flat {
        by_family.entry(row.family.clone()).or_default().push(row);
    }
    // Walk families in natural-sort order so `recall@2.mean`
    // emits before `recall@10.mean`. (BTreeMap is lex-sorted —
    // re-sort the key list instead of switching the bucket
    // type.)
    let mut family_names: Vec<String> = by_family.keys().cloned().collect();
    family_names.sort_by(|a, b| cmp_natural(a, b));
    let mut families = serde_json::Map::new();
    for name in family_names {
        let rows = by_family.remove(&name).unwrap_or_default();
        families.insert(name, build_family_tree(&rows, show_values));
    }
    serde_json::json!({
        "db": db.display().to_string(),
        "families": serde_json::Value::Object(families),
    })
}

/// Build the tree for one family: every label key contributes
/// a level, regardless of how many distinct values it carries
/// in the current result set. Labels with one value just have
/// one branch under that level — uniform traversal beats
/// factoring "constants" out into a separate sibling map.
///
/// Labels are sorted by key per-instance first so siblings at
/// the same depth share the same label key (canonical order),
/// even if the producer declared labels in different orders
/// across instances.
fn build_family_tree(rows: &[&InstanceRow], show_values: bool) -> serde_json::Value {
    // (sorted label set, value summary) pairs feeding the dim tree.
    #[allow(clippy::type_complexity)]
    let normalized: Vec<(Vec<(String, String)>, Option<ValueSummary>)> = rows
        .iter()
        .map(|r| {
            let mut s = r.labels.clone();
            s.sort_by(|a, b| a.0.cmp(&b.0));
            (s, r.values.clone())
        })
        .collect();

    nest_label_tree(&normalized, show_values)
}

/// Recursive nester. At each level, groups rows by their next
/// `(label_key, label_value)` pair and emits the branch as a
/// single `"key=value"` map key — one level per dimension
/// instead of alternating key-level / value-level layers.
/// Leaves (no labels left) emit `{count, mean, …}` in `show`
/// mode, `null` in `list` mode.
// `rows` is the (label set, value summary) list peeled one
// dimension per recursion; the nested tuple is the intrinsic shape.
#[allow(clippy::type_complexity)]
fn nest_label_tree(
    rows: &[(Vec<(String, String)>, Option<ValueSummary>)],
    show_values: bool,
) -> serde_json::Value {
    if rows.is_empty() {
        return serde_json::Value::Null;
    }
    if rows.iter().all(|(l, _)| l.is_empty()) {
        // Pure leaf — typical for the deepest level. Multiple
        // rows colliding here shouldn't happen unless the
        // metric instance set has duplicate label-tuples; if
        // it does, we take the first row's values (BTreeMap
        // dedup should have removed duplicates upstream anyway).
        if show_values {
            let v = rows[0].1.clone().unwrap_or_default();
            return serde_json::Value::String(tree_leaf_summary(&v));
        }
        return serde_json::Value::Null;
    }
    // Bucket rows by their first `(key, value)` pair. The
    // BTreeMap is just for grouping; we re-sort the key tuples
    // naturally before emitting so `limit=2` precedes
    // `limit=10`.
    #[allow(clippy::type_complexity)]
    let mut by_kv: BTreeMap<
        (String, String),
        Vec<(Vec<(String, String)>, Option<ValueSummary>)>,
    > = BTreeMap::new();
    for (labels, values) in rows {
        if labels.is_empty() {
            // Mixed-arity case: some siblings have ended their
            // dim list while others continue. Skip — the
            // emitted tree won't carry this row, but it's a
            // pathological label-shape that doesn't appear in
            // OpenMetrics-conforming families. Surface as
            // structural drift later if it ever does.
            continue;
        }
        let (k, v) = labels[0].clone();
        let rest: Vec<(String, String)> = labels[1..].to_vec();
        by_kv
            .entry((k, v))
            .or_default()
            .push((rest, values.clone()));
    }
    let mut sorted_kvs: Vec<(String, String)> = by_kv.keys().cloned().collect();
    sorted_kvs.sort_by(|a, b| cmp_natural(&a.0, &b.0).then_with(|| cmp_natural(&a.1, &b.1)));
    let mut out = serde_json::Map::new();
    for kv in sorted_kvs {
        let subset = by_kv.remove(&kv).unwrap_or_default();
        let key = format!("{}={}", kv.0, kv.1);
        out.insert(key, nest_label_tree(&subset, show_values));
    }
    serde_json::Value::Object(out)
}

/// One-line text summary used at `--tree` leaves: a compact,
/// scannable encoding of count / time range / canonical
/// statistical moments. Format:
/// `<kind>[N] timespan[<duration>] (min,mean,max,median,stddev)=(...)`,
/// where `<kind>` is `obs` (summary), `total` (counter) or
/// `samples` (gauge/scalar) per [`CountKind`]. The five moments
/// are computed over the value set appropriate to the kind (see
/// [`load_all_value_summaries`]). Unknown fields render as `?` so the
/// format stays positional (a reader can `cut`/`awk` it without a
/// header).
fn tree_leaf_summary(v: &ValueSummary) -> String {
    let n = v.count.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
    let span = match (v.ts_min_ms, v.ts_max_ms) {
        (Some(a), Some(b)) if b >= a => format_duration_ms(b - a),
        _ => "?".into(),
    };
    let f = |x: Option<f64>| x.map(|v| format!("{v}")).unwrap_or_else(|| "?".into());
    format!(
        "{}[{n}] timespan[{span}] (min,mean,max,median,stddev)=({},{},{},{},{})",
        v.count_kind.word(),
        f(v.min),
        f(v.mean),
        f(v.max),
        f(v.p50),
        f(v.stddev),
    )
}

/// Render a millisecond duration as `HhMmSs` / `MmSs` / `Ss` /
/// `Nms` — the largest unit non-zero down to the next, dropping
/// trailing zero units. Stays compact at the leaf-summary level.
fn format_duration_ms(ms: i64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let total_s = ms / 1000;
    let h = total_s / 3600;
    let m = (total_s % 3600) / 60;
    let s = total_s % 60;
    if h > 0 {
        if m == 0 && s == 0 {
            format!("{h}h")
        } else if s == 0 {
            format!("{h}h{m}m")
        } else {
            format!("{h}h{m}m{s}s")
        }
    } else if m > 0 {
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m{s}s")
        }
    } else {
        format!("{s}s")
    }
}

fn render_csv_header(flat: &[InstanceRow], show_values: bool) -> String {
    let keys = union_label_keys(flat);
    let mut header: Vec<String> = vec!["family".into()];
    header.extend(keys.iter().cloned());
    if show_values {
        header.extend(
            ["count", "mean", "p50", "p99", "min", "max"]
                .iter()
                .map(|s| s.to_string()),
        );
    }
    header
        .iter()
        .map(|s| csv_escape(s))
        .collect::<Vec<_>>()
        .join(",")
}

fn render_csv(
    w: &mut dyn std::io::Write,
    flat: &[InstanceRow],
    show_values: bool,
) -> std::io::Result<()> {
    let keys = union_label_keys(flat);
    writeln!(w, "{}", render_csv_header(flat, show_values))?;
    for row in flat {
        let label_map: BTreeMap<&str, &str> = row
            .labels
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let mut cells: Vec<String> = vec![csv_escape(&row.family)];
        for k in &keys {
            cells.push(csv_escape(label_map.get(k.as_str()).copied().unwrap_or("")));
        }
        if show_values {
            let v = row.values.clone().unwrap_or_default();
            cells.push(v.count.map(|x| x.to_string()).unwrap_or_default());
            cells.push(v.mean.map(fmt_f64).unwrap_or_default());
            cells.push(v.p50.map(fmt_f64).unwrap_or_default());
            cells.push(v.p99.map(fmt_f64).unwrap_or_default());
            cells.push(v.min.map(fmt_f64).unwrap_or_default());
            cells.push(v.max.map(fmt_f64).unwrap_or_default());
        }
        writeln!(w, "{}", cells.join(","))?;
    }
    Ok(())
}

fn fmt_f64(x: f64) -> String {
    // Compact human form; CSV consumers parse this fine.
    format!("{x}")
}

fn union_label_keys(flat: &[InstanceRow]) -> Vec<String> {
    let mut keys: BTreeSet<String> = BTreeSet::new();
    for row in flat {
        for (k, _) in &row.labels {
            keys.insert(k.clone());
        }
    }
    keys.into_iter().collect()
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Natural-order compare for label keys/values: numeric runs are
/// compared by integer value (so `"2"` < `"10"` instead of the
/// lexicographic `"10"` < `"2"`), with non-numeric runs falling
/// back to byte-wise compare. Walks both strings in parallel,
/// chunking each into "all digits" / "all non-digits" segments
/// and comparing the corresponding segments numerically when
/// both are digit runs. Handles mixed strings like `"k10"` vs
/// `"k2"` correctly.
fn cmp_natural(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let a = a.as_bytes();
    let b = b.as_bytes();
    let mut i = 0;
    let mut j = 0;
    while i < a.len() && j < b.len() {
        let a_digit = a[i].is_ascii_digit();
        let b_digit = b[j].is_ascii_digit();
        if a_digit && b_digit {
            // Find the digit-run boundaries on both sides.
            let ai = i;
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            let bj = j;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            // Drop leading zeros so "007" == "7" numerically;
            // tiebreak on the original lengths so "07" < "7"
            // sorts the longer-with-leading-zeros after the
            // bare form (stable for IDs like "0001").
            let a_run = std::str::from_utf8(&a[ai..i]).unwrap_or("");
            let b_run = std::str::from_utf8(&b[bj..j]).unwrap_or("");
            let a_trim = a_run.trim_start_matches('0');
            let b_trim = b_run.trim_start_matches('0');
            // Compare by stripped length first (shorter ⇒ smaller),
            // then char-wise; this is the standard "natural" rule.
            match a_trim.len().cmp(&b_trim.len()) {
                Ordering::Equal => match a_trim.cmp(b_trim) {
                    Ordering::Equal => match a_run.len().cmp(&b_run.len()) {
                        Ordering::Equal => continue,
                        non_eq => return non_eq,
                    },
                    non_eq => return non_eq,
                },
                non_eq => return non_eq,
            }
        } else {
            match a[i].cmp(&b[j]) {
                Ordering::Equal => {
                    i += 1;
                    j += 1;
                }
                non_eq => return non_eq,
            }
        }
    }
    a.len().cmp(&b.len())
}

/// Natural-order tuple compare: walks `(key, value)` pairs in
/// parallel using [`cmp_natural`] on each field. Used to sort
/// instances within a family so two instances with `k="2"` /
/// `k="10"` come out as 2, 10 instead of 10, 2.
fn cmp_label_pairs_natural(a: &[(String, String)], b: &[(String, String)]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (av, bv) in a.iter().zip(b.iter()) {
        match cmp_natural(&av.0, &bv.0) {
            Ordering::Equal => match cmp_natural(&av.1, &bv.1) {
                Ordering::Equal => continue,
                non_eq => return non_eq,
            },
            non_eq => return non_eq,
        }
    }
    a.len().cmp(&b.len())
}

/// Split a list of label sets into (constant dims, per-set
/// varying dims). A "constant dim" is a `(key, value)` pair
/// shared by every input set; those are factored out so the
/// tree depth reflects only the dimensions that actually vary.
// Returns (constant label set shared by all, per-input varying
// label sets) — both are plain label-set lists.
#[allow(clippy::type_complexity)]
fn factor_constant_dims(
    label_sets: &[Vec<(String, String)>],
) -> (Vec<(String, String)>, Vec<Vec<(String, String)>>) {
    if label_sets.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let first: BTreeMap<String, String> = label_sets[0].iter().cloned().collect();
    let mut shared: BTreeMap<String, String> = first;
    for set in &label_sets[1..] {
        let cur: BTreeMap<String, String> = set.iter().cloned().collect();
        shared.retain(|k, v| cur.get(k) == Some(v));
        if shared.is_empty() {
            break;
        }
    }
    let const_keys: std::collections::HashSet<String> = shared.keys().cloned().collect();
    let varying: Vec<Vec<(String, String)>> = label_sets
        .iter()
        .map(|s| {
            s.iter()
                .filter(|(k, _)| !const_keys.contains(k))
                .cloned()
                .collect()
        })
        .collect();
    let constant: Vec<(String, String)> = shared.into_iter().collect();
    (constant, varying)
}

#[derive(Debug, Clone, PartialEq)]
struct LabelMatcher {
    /// Family glob pattern (`*` allowed); `None` means match-all.
    family: Option<String>,
    /// Per-label match: equals, regex/substring (`~`), or
    /// just-presence.
    labels: Vec<(String, LabelMatch)>,
}

#[derive(Debug, Clone, PartialEq)]
enum LabelMatch {
    Equals(String),
    Substring(String),
}

impl LabelMatcher {
    fn matches(&self, family: &str, labels: &[(String, String)]) -> bool {
        if let Some(g) = self.family.as_deref()
            && !glob_matches(g, family)
        {
            return false;
        }
        for (k, want) in &self.labels {
            let v = labels.iter().find(|(lk, _)| lk == k).map(|(_, v)| v);
            let ok = match (v, want) {
                (Some(v), LabelMatch::Equals(e)) => v == e,
                (Some(v), LabelMatch::Substring(s)) => v.contains(s),
                (None, _) => false,
            };
            if !ok {
                return false;
            }
        }
        true
    }
}

/// Parse a metric filter expression. Accepted shapes:
///   - `family_glob`
///   - `family_glob{label="value", label2=~substring}`
///   - `{label="value"}` (label-only filter, any family)
fn parse_filter(expr: &str) -> Result<LabelMatcher, String> {
    let expr = expr.trim();
    let (family_part, labels_part) = match expr.find('{') {
        Some(i) => (expr[..i].trim(), Some(expr[i..].to_string())),
        None => (expr, None),
    };
    let family = if family_part.is_empty() {
        None
    } else {
        Some(family_part.to_string())
    };
    let mut labels: Vec<(String, LabelMatch)> = Vec::new();
    if let Some(lp) = labels_part {
        let lp = lp.trim();
        let inner = lp
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .ok_or_else(|| "label block must be `{...}`".to_string())?;
        for raw in inner.split(',') {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let (key, op_val) = if let Some((k, v)) = raw.split_once("=~") {
                (
                    k.trim().to_string(),
                    LabelMatch::Substring(unquote(v.trim()).to_string()),
                )
            } else if let Some((k, v)) = raw.split_once('=') {
                (
                    k.trim().to_string(),
                    LabelMatch::Equals(unquote(v.trim()).to_string()),
                )
            } else if let Some((k, v)) = raw.split_once('~') {
                (
                    k.trim().to_string(),
                    LabelMatch::Substring(unquote(v.trim()).to_string()),
                )
            } else {
                return Err(format!(
                    "label clause '{raw}': expected `key=value` or `key=~substring`"
                ));
            };
            labels.push((key, op_val));
        }
    }
    Ok(LabelMatcher { family, labels })
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))
        .unwrap_or(s)
}

fn glob_matches(glob: &str, name: &str) -> bool {
    fn rec(g: &[u8], n: &[u8]) -> bool {
        match (g.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => {
                if rec(&g[1..], n) {
                    return true;
                }
                if !n.is_empty() && rec(g, &n[1..]) {
                    return true;
                }
                false
            }
            (Some(b'?'), Some(_)) => rec(&g[1..], &n[1..]),
            (Some(gc), Some(nc)) if gc == nc => rec(&g[1..], &n[1..]),
            _ => false,
        }
    }
    rec(glob.as_bytes(), name.as_bytes())
}

pub(crate) fn split_spec(spec: &str) -> (String, Vec<(String, String)>) {
    let (family, labels_text) = match spec.find('{') {
        Some(i) => (spec[..i].to_string(), &spec[i + 1..]),
        None => return (spec.to_string(), Vec::new()),
    };
    let inner = labels_text.strip_suffix('}').unwrap_or(labels_text);
    let mut out = Vec::new();
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b',') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let key = inner[key_start..i].trim().to_string();
        i += 1;
        if i < bytes.len() && bytes[i] == b'"' {
            i += 1;
            let vs = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            let val = inner[vs..i].to_string();
            if i < bytes.len() {
                i += 1;
            }
            out.push((key, val));
        } else {
            let vs = i;
            while i < bytes.len() && !matches!(bytes[i], b',') {
                i += 1;
            }
            out.push((key, inner[vs..i].trim().to_string()));
        }
    }
    (family, out)
}

/// One node in the dimensional tree. Ordered by label-key
/// occurrence so deeper levels reflect which dimensions vary.
#[derive(Debug, Default)]
struct DimNode {
    /// Distinct label-tuple paths reaching this node.
    leaves: Vec<Vec<(String, String)>>,
    /// Children keyed by `(label_key, label_value)`.
    children: BTreeMap<(String, String), DimNode>,
}

fn build_dim_tree(label_sets: Vec<Vec<(String, String)>>) -> DimNode {
    let mut root = DimNode::default();
    for ls in label_sets {
        insert_into_dim_tree(&mut root, &ls, 0);
    }
    root
}

fn insert_into_dim_tree(node: &mut DimNode, labels: &[(String, String)], depth: usize) {
    if depth >= labels.len() {
        node.leaves.push(labels.to_vec());
        return;
    }
    let (k, v) = &labels[depth];
    let child = node.children.entry((k.clone(), v.clone())).or_default();
    insert_into_dim_tree(child, labels, depth + 1);
}

fn write_dim_tree(
    w: &mut dyn std::io::Write,
    node: &DimNode,
    indent: &str,
    instances: &BTreeMap<Vec<(String, String)>, i64>,
    summaries: &std::collections::HashMap<i64, ValueSummary>,
    scopes: &std::collections::HashMap<i64, String>,
    show_values: bool,
) -> std::io::Result<()> {
    // Iterate in natural order: by label key first, then by
    // value (so `k=2` comes before `k=10`). The underlying
    // BTreeMap is keyed (k, v); a lex iteration would put
    // `"10"` before `"2"`. Build the sorted key list once.
    let n_children = node.children.len();
    let mut sorted_keys: Vec<&(String, String)> = node.children.keys().collect();
    sorted_keys.sort_by(|a, b| cmp_natural(&a.0, &b.0).then_with(|| cmp_natural(&a.1, &b.1)));
    for (idx, kv) in sorted_keys.iter().enumerate() {
        let (k, v) = (&kv.0, &kv.1);
        let child = node.children.get(*kv).unwrap();
        let is_last = idx + 1 == n_children;
        let connector = if is_last { "└── " } else { "├── " };
        let next_indent = if is_last {
            format!("{indent}    ")
        } else {
            format!("{indent}│   ")
        };
        // Leaf detection: any node with at least one leaf at
        // *this exact level* prints its summary inline.
        let inline_leaf: Option<&Vec<(String, String)>> = child.leaves.iter().find(|ls| {
            ls.last()
                .map(|kv| kv == &(k.clone(), v.clone()))
                .unwrap_or(false)
        });

        if let Some(ls) = inline_leaf
            && child.children.is_empty()
        {
            let id = instances.get(ls).copied().unwrap_or(-1);
            let summary = if show_values {
                let vs = summaries.get(&id).cloned().unwrap_or_default();
                format!("  {}", value_summary_string(&vs))
            } else {
                String::new()
            };
            let scope = scopes
                .get(&id)
                .map(|a| format!("  [{a}]"))
                .unwrap_or_default();
            writeln!(w, "{indent}{connector}{k}={v}{summary}{scope}")?;
        } else {
            writeln!(w, "{indent}{connector}{k}={v}")?;
            write_dim_tree(
                w,
                child,
                &next_indent,
                instances,
                summaries,
                scopes,
                show_values,
            )?;
        }
    }
    Ok(())
}

/// Which statistic semantics a leaf's [`ValueSummary`] should
/// carry, decided by the metric family's declared type.
///
/// The five moments (`min/mean/max/median/stddev`) aren't quantiles —
/// only the median is — so they're well-defined over any scalar
/// series:
///
/// - **summary** → stored reservoir moments from the most-observed
///   window (within-window observation distribution); `count` is
///   the cumulative observation count (`obs`).
/// - **gauge / scalar** → moments over the *series of readings*
///   (the `mean` column across windows); `count` is the number of
///   readings (`samples`).
/// - **counter / histogram / info** → moments over the *per-window
///   increments* of the cumulative `count` column (throughput-
///   per-window distribution); `count` is the cumulative total
///   (`total`).
#[derive(Debug, Clone, Copy)]
enum LeafKind {
    /// `summary` — the stored HDR-reservoir moments describe the
    /// observation distribution within a window.
    Distribution,
    /// `gauge` / `stateset` / unknown — the stored scalar lives in
    /// the `mean` column and each row is one reading.
    Gauge,
    /// `counter` / `histogram` / `info` — the `count` column is a
    /// cumulative total; the meaningful distribution is over its
    /// per-window increments.
    Counter,
}

impl LeafKind {
    /// Map a `metric_family.type` string to the leaf statistic kind.
    fn from_family_type(ty: &str) -> Self {
        match ty {
            "summary" => LeafKind::Distribution,
            "counter" | "histogram" | "gaugehistogram" | "info" => LeafKind::Counter,
            // gauge, stateset, unknown, and anything else: each
            // row's `mean` is one scalar reading.
            _ => LeafKind::Gauge,
        }
    }
}

/// SRD-93 stage 1 — every requested instance's [`ValueSummary`] from
/// ONE ordered pass over `sample_value` (O(table) total, index-free —
/// a live session's db has its read indexes deferred to shutdown),
/// replacing the per-instance query storm that scaled
/// O(instances × table) and produced the 76-minute `metrics show`.
/// Rows stream grouped by `instance_id`; each group finalizes through
/// the same per-kind statistics as the old per-instance path.
/// Instances in `ids` with no sample rows get the kind-appropriate
/// empty summary.
fn load_all_value_summaries(
    conn: &rusqlite::Connection,
    ids: &std::collections::HashSet<i64>,
) -> std::collections::HashMap<i64, ValueSummary> {
    use std::collections::HashMap;

    // Family kind per instance — one batched join, not one query
    // per instance.
    let mut kinds: HashMap<i64, LeafKind> = HashMap::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT mi.id, mf.type FROM metric_instance mi \
         JOIN metric_family mf ON mi.family_id = mf.id",
    ) && let Ok(it) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
    {
        for (id, ty) in it.flatten() {
            kinds.insert(id, LeafKind::from_family_type(&ty));
        }
    }

    /// Streaming per-group state: raw material for every kind, so
    /// the row loop stays branch-light and finalize picks by kind.
    #[derive(Default)]
    struct Accum {
        ts_min: Option<i64>,
        ts_max: Option<i64>,
        /// Gauge readings — the `mean` column per row, in ts order.
        readings: Vec<f64>,
        /// Cumulative counter values — the `count` column per row.
        cum: Vec<f64>,
        /// Peak-`count` reservoir row (summary families): the stored
        /// moments from the most-observed window.
        peak: Option<(i64, ValueSummary)>,
    }

    fn finalize(kind: LeafKind, acc: Accum) -> ValueSummary {
        let (ts_min_ms, ts_max_ms) = (acc.ts_min, acc.ts_max);
        match kind {
            LeafKind::Distribution => match acc.peak {
                Some((_, mut v)) => {
                    v.ts_min_ms = ts_min_ms;
                    v.ts_max_ms = ts_max_ms;
                    v
                }
                None => ValueSummary {
                    count_kind: CountKind::Obs,
                    ts_min_ms,
                    ts_max_ms,
                    ..Default::default()
                },
            },
            LeafKind::Gauge => {
                let n = Some(acc.readings.len() as i64);
                summary_from_values(acc.readings, n, CountKind::Samples, ts_min_ms, ts_max_ms)
            }
            LeafKind::Counter => {
                let total = if acc.cum.is_empty() {
                    None
                } else {
                    Some(acc.cum.iter().cloned().fold(f64::NEG_INFINITY, f64::max) as i64)
                };
                let deltas = window_increments(&acc.cum);
                summary_from_values(deltas, total, CountKind::Total, ts_min_ms, ts_max_ms)
            }
        }
    }

    let mut out: HashMap<i64, ValueSummary> = HashMap::new();
    // Backfill: any requested instance the scan produced no summary
    // for (no sample rows, or a prepare/query error) gets the
    // kind-appropriate empty summary — the old per-instance path's
    // output for a data-less instance.
    let backfill = |out: &mut HashMap<i64, ValueSummary>| {
        for &id in ids {
            if !out.contains_key(&id) {
                let kind = kinds.get(&id).copied().unwrap_or(LeafKind::Gauge);
                out.insert(id, finalize(kind, Accum::default()));
            }
        }
    };

    let mut stmt = match conn.prepare(
        "SELECT instance_id, timestamp_ms, count, mean, \
                p50, p99, min, max, stddev \
         FROM sample_value ORDER BY instance_id, timestamp_ms",
    ) {
        Ok(s) => s,
        Err(_) => {
            backfill(&mut out);
            return out;
        }
    };
    #[allow(clippy::type_complexity)]
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,         // instance_id
            r.get::<_, i64>(1)?,         // timestamp_ms
            r.get::<_, Option<i64>>(2)?, // count
            r.get::<_, Option<f64>>(3)?, // mean
            r.get::<_, Option<f64>>(4)?, // p50
            r.get::<_, Option<f64>>(5)?, // p99
            r.get::<_, Option<f64>>(6)?, // min
            r.get::<_, Option<f64>>(7)?, // max
            r.get::<_, Option<f64>>(8)?, // stddev
        ))
    });

    let mut cur_id: Option<i64> = None;
    let mut cur_kind = LeafKind::Gauge;
    let mut cur_wanted = false;
    let mut acc = Accum::default();
    if let Ok(rows) = rows {
        for (id, ts, count, mean, p50, p99, min, max, stddev) in rows.flatten() {
            if cur_id != Some(id) {
                if let Some(pid) = cur_id
                    && cur_wanted
                {
                    out.insert(pid, finalize(cur_kind, std::mem::take(&mut acc)));
                } else {
                    acc = Accum::default();
                }
                cur_id = Some(id);
                cur_kind = kinds.get(&id).copied().unwrap_or(LeafKind::Gauge);
                cur_wanted = ids.contains(&id);
            }
            if !cur_wanted {
                continue;
            }
            acc.ts_min = Some(acc.ts_min.map_or(ts, |t| t.min(ts)));
            acc.ts_max = Some(acc.ts_max.map_or(ts, |t| t.max(ts)));
            match cur_kind {
                LeafKind::Gauge => {
                    if let Some(m) = mean {
                        acc.readings.push(m);
                    }
                }
                LeafKind::Counter => {
                    if let Some(c) = count {
                        acc.cum.push(c as f64);
                    }
                }
                LeafKind::Distribution => {
                    // Mirror `ORDER BY count DESC LIMIT 1`: highest
                    // non-null count wins; NULL counts rank lowest.
                    let rank = count.unwrap_or(i64::MIN);
                    let better = acc.peak.as_ref().map_or(true, |(best, _)| rank > *best);
                    if better {
                        acc.peak = Some((
                            rank,
                            ValueSummary {
                                count,
                                count_kind: CountKind::Obs,
                                mean,
                                p50,
                                p99,
                                min,
                                max,
                                stddev,
                                ts_min_ms: None,
                                ts_max_ms: None,
                            },
                        ));
                    }
                }
            }
        }
    }
    if let Some(pid) = cur_id
        && cur_wanted
    {
        out.insert(pid, finalize(cur_kind, acc));
    }

    backfill(&mut out);
    out
}

/// Per-window increments of a cumulative series, each clamped at
/// 0 so a counter reset contributes 0 rather than a negative
/// spike. The first window's increment is measured from 0.
fn window_increments(cum: &[f64]) -> Vec<f64> {
    let mut out = Vec::with_capacity(cum.len());
    let mut prev = 0.0;
    for &c in cum {
        out.push((c - prev).max(0.0));
        prev = c;
    }
    out
}

/// Five canonical moments over a value set. Median and p99 use
/// nearest-rank on the sorted values; stddev is population.
fn summary_from_values(
    mut xs: Vec<f64>,
    count: Option<i64>,
    count_kind: CountKind,
    ts_min_ms: Option<i64>,
    ts_max_ms: Option<i64>,
) -> ValueSummary {
    if xs.is_empty() {
        return ValueSummary {
            count,
            count_kind,
            ts_min_ms,
            ts_max_ms,
            ..Default::default()
        };
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = xs.len();
    let mean = xs.iter().sum::<f64>() / n as f64;
    let var = xs
        .iter()
        .map(|x| {
            let d = x - mean;
            d * d
        })
        .sum::<f64>()
        / n as f64;
    let pct = |q: f64| xs[(((n - 1) as f64) * q).round() as usize];
    ValueSummary {
        count,
        count_kind,
        mean: Some(mean),
        p50: Some(pct(0.50)),
        p99: Some(pct(0.99)),
        min: Some(xs[0]),
        max: Some(xs[n - 1]),
        stddev: Some(var.sqrt()),
        ts_min_ms,
        ts_max_ms,
    }
}

/// SRD-93 stage 5 — pre-rendered lifecycle annotation per instance
/// from `instance_scope_event`:
///
/// - `exited <reason> @+<t>s` — an exit event exists; `t` is session
///   time (nanos of session → seconds).
/// - `no clean exit` — enter without exit AND the owning execution
///   has ended: the truthful crash/interrupt marker (SRD-93 A7).
/// - `in-scope` — enter without exit, execution still in flight.
///
/// A db that predates the table (or an errored query) yields an
/// empty map — the annotation column simply doesn't render.
fn load_scope_annotations(conn: &rusqlite::Connection) -> std::collections::HashMap<i64, String> {
    let mut out = std::collections::HashMap::new();
    let sql = "SELECT e.instance_id, \
                      MAX(CASE WHEN e.event = 'exit' THEN e.reason END), \
                      MAX(CASE WHEN e.event = 'exit' \
                          THEN e.at_session_nanos END), \
                      MAX(x.ended_at_nanos IS NOT NULL) \
               FROM instance_scope_event e \
               JOIN executions x \
                 ON x.session = e.session AND x.exec_id = e.exec_id \
               GROUP BY e.instance_id";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return out;
    };
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<i64>>(2)?,
            r.get::<_, i64>(3)?,
        ))
    });
    if let Ok(rows) = rows {
        for (id, exit_reason, exit_session_ns, exec_ended) in rows.flatten() {
            let ann = match (exit_reason, exit_session_ns) {
                (Some(reason), Some(ns)) => {
                    format!("exited {reason} @+{:.1}s", ns as f64 / 1e9)
                }
                _ if exec_ended != 0 => "no clean exit".to_string(),
                _ => "in-scope".to_string(),
            };
            out.insert(id, ann);
        }
    }
    out
}

/// Render one leaf's summary inline — `(total=…, mean=…, …)`.
fn value_summary_string(v: &ValueSummary) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(c) = v.count {
        parts.push(format!("{}={c}", v.count_kind.word()));
    }
    if let Some(m) = v.mean {
        parts.push(format!("mean={m:.4}"));
    }
    if let Some(p) = v.p50 {
        parts.push(format!("p50={p:.4}"));
    }
    if let Some(p) = v.p99 {
        parts.push(format!("p99={p:.4}"));
    }
    if let (Some(mn), Some(mx)) = (v.min, v.max) {
        parts.push(format!("[{mn:.4}..{mx:.4}]"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("({})", parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(positionals: &[&str], flags: &[(&str, &str)]) -> ParsedCommand {
        let mut f: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for (k, v) in flags {
            f.entry((*k).to_string())
                .or_default()
                .push((*v).to_string());
        }
        ParsedCommand {
            path: vec!["metrics".into(), "match".into()],
            flags: f,
            bools: Default::default(),
            positionals: positionals.iter().map(|s| s.to_string()).collect(),
            raw: Vec::new(),
            argv: Vec::new(),
            help_requested: false,
            version_requested: false,
        }
    }

    /// Declaring a flag and FORWARDING it are separate obligations. These
    /// handlers rebuild an argv for their imperative parser, and the session
    /// family was left out of that rebuild — so `--session` passed the walker,
    /// was dropped before `resolve_db` saw it, and the command read the latest
    /// session while naming another.
    #[test]
    fn session_flags_reach_the_legacy_parser() {
        let mut argv = Vec::new();
        forward_session_flags(&parsed(&["expr"], &[("--session", "/tmp/s")]), &mut argv);
        assert_eq!(argv, vec!["--session".to_string(), "/tmp/s".to_string()]);

        let mut argv = Vec::new();
        forward_session_flags(
            &parsed(&["expr"], &[("--session-path", "/tmp/p")]),
            &mut argv,
        );
        assert_eq!(
            argv,
            vec!["--session-path".to_string(), "/tmp/p".to_string()]
        );
    }

    /// The bare `session=<dir>` spelling arrives as a POSITIONAL on a
    /// walker-parsed command. Unforwarded it was silently swallowed and the
    /// command read the latest session — the same silent-wrong-answer as the
    /// `--session` gap. The report/summary family already accepts this spelling.
    #[test]
    fn bare_session_positional_is_forwarded_and_kept_out_of_the_expr() {
        let p = parsed(&["zzprobe*", "session=/tmp/s"], &[]);
        let mut argv = Vec::new();
        forward_session_flags(&p, &mut argv);
        assert_eq!(argv, vec!["--session".to_string(), "/tmp/s".to_string()]);
        assert_eq!(
            expr_positional(&p),
            Some("zzprobe*"),
            "the pattern must still be the expression"
        );

        // Order must not matter: with the session token FIRST, the expression is
        // still the pattern — otherwise `session=…` becomes the pattern and
        // matches nothing.
        let p = parsed(&["session=/tmp/s", "zzprobe*"], &[]);
        assert_eq!(expr_positional(&p), Some("zzprobe*"));
        let mut argv = Vec::new();
        forward_session_flags(&p, &mut argv);
        assert_eq!(argv, vec!["--session".to_string(), "/tmp/s".to_string()]);

        // No session token ⇒ nothing forwarded, expression unchanged.
        let p = parsed(&["zzprobe*"], &[]);
        let mut argv = Vec::new();
        forward_session_flags(&p, &mut argv);
        assert!(argv.is_empty());
        assert_eq!(expr_positional(&p), Some("zzprobe*"));
    }

    /// `resolve_db` prefers an explicit `--db`, then the named session, then the
    /// latest. The middle step is the one that was missing.
    #[test]
    fn resolve_db_prefers_db_then_session() {
        let explicit = resolve_db(Some(PathBuf::from("/tmp/x.db")), &[]);
        assert_eq!(explicit, PathBuf::from("/tmp/x.db"));

        let dir =
            std::env::temp_dir().join(format!("nmbrs-resolve-db-{}", crate::scratch_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        let args = vec![format!("--session={}", dir.display())];
        assert_eq!(
            resolve_db(None, &args),
            dir.join("metrics.db"),
            "a named session must supply the db when no --db is given"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    use std::cmp::Ordering;

    #[test]
    fn natural_order_pure_numeric() {
        assert_eq!(cmp_natural("2", "10"), Ordering::Less);
        assert_eq!(cmp_natural("10", "2"), Ordering::Greater);
        assert_eq!(cmp_natural("100", "100"), Ordering::Equal);
        assert_eq!(cmp_natural("1000", "100"), Ordering::Greater);
    }

    #[test]
    fn natural_order_with_prefix() {
        // Mixed strings: prefix-equal, numeric tail compares by value.
        assert_eq!(cmp_natural("k2", "k10"), Ordering::Less);
        assert_eq!(
            cmp_natural("recall@2.mean", "recall@10.mean"),
            Ordering::Less
        );
    }

    #[test]
    fn natural_order_leading_zeros() {
        // "007" == "7" numerically; the longer leading-zero form
        // sorts AFTER the bare form so iteration is stable.
        assert_eq!(cmp_natural("7", "007"), Ordering::Less);
        assert_eq!(cmp_natural("007", "7"), Ordering::Greater);
    }

    #[test]
    fn natural_order_non_numeric_falls_back_to_lex() {
        assert_eq!(cmp_natural("alpha", "beta"), Ordering::Less);
        assert_eq!(cmp_natural("foo", "foo"), Ordering::Equal);
    }

    #[test]
    fn natural_sort_ordering_demo() {
        let mut v = vec!["10", "1", "2", "100", "20"];
        v.sort_by(|a, b| cmp_natural(a, b));
        assert_eq!(v, vec!["1", "2", "10", "20", "100"]);
    }

    #[test]
    fn render_metricsql_sorts_labels_naturally() {
        // Producer-order labels (as parsed from metric_instance.spec)
        // should re-emerge in natural-alphanumeric key order on
        // the metricsql line — every line shares the same key
        // sequence so columnar readers / diff tools align.
        let row1 = InstanceRow {
            family: "recall".into(),
            labels: vec![
                ("profile".into(), "label_03".into()),
                ("k".into(), "10".into()),
                ("limit".into(), "50".into()),
                ("k_at_test".into(), "1".into()),
            ],
            values: None,
        };
        let row2 = InstanceRow {
            family: "recall".into(),
            labels: vec![
                ("limit".into(), "100".into()),
                ("k".into(), "1".into()),
                ("profile".into(), "label_00".into()),
                ("k_at_test".into(), "10".into()),
            ],
            values: None,
        };
        let mut buf: Vec<u8> = Vec::new();
        render_metricsql(&mut buf, &[row1, row2]).unwrap();
        let out = String::from_utf8(buf).unwrap();
        // Both lines share the same key sequence:
        // k, k_at_test, limit, profile (natural alpha-num).
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            r#"recall{k="10",k_at_test="1",limit="50",profile="label_03"}"#
        );
        assert_eq!(
            lines[1],
            r#"recall{k="1",k_at_test="10",limit="100",profile="label_00"}"#
        );
    }

    #[test]
    fn render_metricsql_escapes_special_chars() {
        let row = InstanceRow {
            family: "weird".into(),
            labels: vec![
                ("path".into(), r#"a"b\c"#.into()),
                ("note".into(), "line1\nline2".into()),
            ],
            values: None,
        };
        let mut buf: Vec<u8> = Vec::new();
        render_metricsql(&mut buf, &[row]).unwrap();
        let out = String::from_utf8(buf).unwrap();
        // Backslash → \\, double-quote → \", newline → \n.
        // Keys appear in natural-alphanumeric order: note, path.
        assert_eq!(
            out.trim_end(),
            r#"weird{note="line1\nline2",path="a\"b\\c"}"#
        );
    }

    #[test]
    fn render_metricsql_empty_labels() {
        // Family with no labels emits just the bare name.
        let row = InstanceRow {
            family: "stanzas_total".into(),
            labels: vec![],
            values: None,
        };
        let mut buf: Vec<u8> = Vec::new();
        render_metricsql(&mut buf, &[row]).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap().trim_end(), "stanzas_total");
    }

    #[test]
    fn format_parse_metricsql_is_canonical_and_synonyms_rejected() {
        // `metricsql` is the one canonical name (case-insensitive); the
        // former openmetrics / promql / prometheus synonyms are gone.
        for ok in &["metricsql", "MetricsQL", "METRICSQL"] {
            assert_eq!(
                Format::parse(ok).unwrap(),
                Format::MetricsQL,
                "'{ok}' must resolve to Format::MetricsQL"
            );
        }
        for gone in &["openmetrics", "promql", "prometheus"] {
            assert!(
                Format::parse(gone).is_err(),
                "synonym '{gone}' must be rejected — metricsql is canonical"
            );
        }
    }

    #[test]
    fn split_spec_basic() {
        let (f, l) = split_spec(r#"recall@10.mean{profile="label_03",k="10",limit="50"}"#);
        assert_eq!(f, "recall@10.mean");
        assert_eq!(
            l,
            vec![
                ("profile".into(), "label_03".into()),
                ("k".into(), "10".into()),
                ("limit".into(), "50".into()),
            ]
        );
    }

    #[test]
    fn parse_filter_family_only() {
        let m = parse_filter("recall*").unwrap();
        assert_eq!(m.family.as_deref(), Some("recall*"));
        assert!(m.labels.is_empty());
    }

    #[test]
    fn parse_filter_label_eq() {
        let m = parse_filter(r#"recall{k="10"}"#).unwrap();
        assert_eq!(m.family.as_deref(), Some("recall"));
        assert_eq!(m.labels.len(), 1);
        assert!(matches!(m.labels[0].1, LabelMatch::Equals(ref s) if s == "10"));
    }

    #[test]
    fn parse_filter_label_substring_em() {
        let m = parse_filter(r#"{profile=~label}"#).unwrap();
        assert!(m.family.is_none());
        assert!(matches!(m.labels[0].1, LabelMatch::Substring(ref s) if s == "label"));
    }

    #[test]
    fn parse_filter_label_substring_tilde_only() {
        let m = parse_filter("{profile~label}").unwrap();
        assert!(matches!(m.labels[0].1, LabelMatch::Substring(ref s) if s == "label"));
    }

    #[test]
    fn matcher_substring_matches() {
        let m = parse_filter(r#"{profile=~label}"#).unwrap();
        assert!(m.matches("any", &[("profile".into(), "label_03".into())]));
        assert!(!m.matches("any", &[("profile".into(), "default".into())]));
    }

    // ── leaf summary statistics (type-aware) ──────────────────

    #[test]
    fn counter_increments_measured_from_zero() {
        // Cumulative counter snapshots → per-window increments,
        // first window measured from 0.
        assert_eq!(
            window_increments(&[10.0, 30.0, 60.0]),
            vec![10.0, 20.0, 30.0]
        );
    }

    #[test]
    fn counter_reset_clamped_to_zero() {
        // A cumulative drop (counter reset) contributes 0, not a
        // negative spike.
        assert_eq!(
            window_increments(&[50.0, 10.0, 25.0]),
            vec![50.0, 0.0, 15.0]
        );
    }

    #[test]
    fn moments_over_value_set() {
        let v = summary_from_values(
            vec![10.0, 20.0, 30.0],
            Some(60),
            CountKind::Total,
            Some(0),
            Some(1000),
        );
        assert_eq!(v.count, Some(60));
        assert_eq!(v.count_kind, CountKind::Total);
        assert_eq!(v.min, Some(10.0));
        assert_eq!(v.max, Some(30.0));
        assert_eq!(v.mean, Some(20.0));
        assert_eq!(v.p50, Some(20.0));
        // population stddev = sqrt(((100+0+100)/3))
        let sd = v.stddev.unwrap();
        assert!((sd - (200.0_f64 / 3.0).sqrt()).abs() < 1e-9, "stddev={sd}");
    }

    #[test]
    fn empty_series_yields_unknown_moments_but_keeps_kind() {
        let v = summary_from_values(vec![], None, CountKind::Samples, None, None);
        assert!(v.min.is_none() && v.mean.is_none() && v.p50.is_none() && v.stddev.is_none());
        assert_eq!(v.count_kind, CountKind::Samples);
    }

    #[test]
    fn tree_leaf_word_and_positions_match_kind() {
        let v = ValueSummary {
            count: Some(5),
            count_kind: CountKind::Total,
            min: Some(1.0),
            mean: Some(2.0),
            max: Some(3.0),
            p50: Some(2.0),
            stddev: Some(0.5),
            ts_min_ms: Some(0),
            ts_max_ms: Some(2000),
            ..Default::default()
        };
        let s = tree_leaf_summary(&v);
        assert!(s.starts_with("total[5] timespan["), "{s}");
        assert!(
            s.ends_with("(min,mean,max,median,stddev)=(1,2,3,2,0.5)"),
            "{s}"
        );
    }

    #[test]
    fn count_kind_words() {
        assert_eq!(CountKind::Samples.word(), "samples");
        assert_eq!(CountKind::Total.word(), "total");
        assert_eq!(CountKind::Obs.word(), "obs");
    }

    /// SRD-93 stage 1 — the one-pass loader reproduces the retired
    /// per-instance path's semantics for every leaf kind: gauge
    /// moments over the readings series, counter moments over
    /// per-window increments with a cumulative total, summary
    /// moments from the peak-count reservoir row, and an instance
    /// with no samples gets the kind-appropriate empty summary.
    #[test]
    fn one_pass_summaries_match_per_kind_semantics() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE metric_family (
                 id INTEGER PRIMARY KEY, name TEXT, type TEXT);
             CREATE TABLE metric_instance (
                 id INTEGER PRIMARY KEY, family_id INTEGER, spec TEXT);
             CREATE TABLE sample_value (
                 instance_id INTEGER, timestamp_ms INTEGER,
                 count INTEGER, mean REAL, p50 REAL, p99 REAL,
                 min REAL, max REAL, stddev REAL);
             INSERT INTO metric_family VALUES
                 (1,'g','gauge'), (2,'c','counter'), (3,'s','summary');
             INSERT INTO metric_instance VALUES
                 (10,1,'g{}'), (20,2,'c{}'), (30,3,'s{}'), (40,1,'g2{}');
             -- gauge readings 1,3,5 (mean column)
             INSERT INTO sample_value VALUES
                 (10, 1000, NULL, 1.0, NULL,NULL,NULL,NULL,NULL),
                 (10, 2000, NULL, 3.0, NULL,NULL,NULL,NULL,NULL),
                 (10, 3000, NULL, 5.0, NULL,NULL,NULL,NULL,NULL);
             -- counter cumulative 5,15,15 → increments 5,10,0
             INSERT INTO sample_value VALUES
                 (20, 1000, 5,  NULL, NULL,NULL,NULL,NULL,NULL),
                 (20, 2000, 15, NULL, NULL,NULL,NULL,NULL,NULL),
                 (20, 3000, 15, NULL, NULL,NULL,NULL,NULL,NULL);
             -- summary: peak-count row (count=90) carries the moments
             INSERT INTO sample_value VALUES
                 (30, 1000, 40, 1.0, 1.5, 9.0, 0.5, 10.0, 0.1),
                 (30, 2000, 90, 2.0, 2.5, 9.9, 0.4, 11.0, 0.2);",
        )
        .unwrap();

        let ids: std::collections::HashSet<i64> = [10, 20, 30, 40].into_iter().collect();
        let out = load_all_value_summaries(&conn, &ids);

        let g = &out[&10];
        assert_eq!(g.count_kind, CountKind::Samples);
        assert_eq!(g.count, Some(3));
        assert_eq!(g.mean, Some(3.0));
        assert_eq!((g.min, g.max), (Some(1.0), Some(5.0)));
        assert_eq!((g.ts_min_ms, g.ts_max_ms), (Some(1000), Some(3000)));

        let c = &out[&20];
        assert_eq!(c.count_kind, CountKind::Total);
        assert_eq!(
            c.count,
            Some(15),
            "counter headline is the cumulative total"
        );
        assert_eq!(c.mean, Some(5.0), "moments are over increments 5,10,0");
        assert_eq!((c.min, c.max), (Some(0.0), Some(10.0)));

        let s = &out[&30];
        assert_eq!(s.count_kind, CountKind::Obs);
        assert_eq!(s.count, Some(90), "peak-count window wins");
        assert_eq!(s.p99, Some(9.9));
        assert_eq!(
            (s.ts_min_ms, s.ts_max_ms),
            (Some(1000), Some(2000)),
            "timespan covers the whole series, not just the peak row"
        );

        let empty = &out[&40];
        assert_eq!(empty.count_kind, CountKind::Samples);
        assert_eq!(empty.count, Some(0), "gauge with no rows reads 0 samples");
        assert_eq!(empty.mean, None);
    }
}
