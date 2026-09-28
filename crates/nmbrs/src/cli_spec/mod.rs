// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Single source of truth for the nmbrs CLI surface.
//!
//! Every nmbrs subcommand declares its shape — name, flags,
//! value providers, positionals, handler — as a [`Command`]
//! tree. From that one declaration we derive:
//!
//! 1. The runtime parser ([`walker::parse`]) — walks argv,
//!    matches the subcommand path, collects flag values, and
//!    returns a [`ParsedCommand`] for the matched leaf's
//!    handler.
//! 2. The shell-completion tree
//!    ([`completion::build_command_tree`]) — produces the
//!    `veks_completion::CommandTree` consumed by tab.
//! 3. Help text rendering ([`help::render_usage`]).
//!
//! Adding a flag is one edit (the spec); the parser, completion,
//! and help all see it automatically. No more shadow lists.
//!
//! Async commands declare [`Handler::Async`]; main.rs spins up
//! the tokio runtime *after* parsing and only when the matched
//! handler is async. Synchronous commands never touch tokio.

pub mod completion;
pub mod help;
pub mod root;
pub mod walker;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;

/// Synchronous command handler. Returns a string error so
/// main.rs can format and exit uniformly; handlers should
/// never call [`std::process::exit`] directly.
pub type SyncHandler = fn(ParsedCommand) -> Result<(), String>;

/// Async command handler. Returns a boxed `Future` so the
/// dispatcher can store handlers homogeneously in [`Handler`]
/// regardless of their concrete future type. main.rs builds
/// the tokio runtime lazily — only when an `Async` handler is
/// matched does the runtime start.
pub type AsyncHandler = fn(ParsedCommand) -> Pin<Box<dyn Future<Output = Result<(), String>>>>;

/// Provider of context-sensitive option tokens for a command,
/// taking the current word and the prior tokens on the line and
/// returning the candidate completions.
pub type DynamicOptions = fn(&str, &[&str]) -> Vec<String>;

/// Tag identifying whether a [`Command`]'s handler is sync or
/// async. The variant doubles as the handler value itself.
#[derive(Clone, Copy)]
pub enum Handler {
    Sync(SyncHandler),
    Async(AsyncHandler),
}

/// One node in the CLI surface — a command, optionally with
/// flags, positionals, and subcommands.
pub struct Command {
    pub name: &'static str,
    pub help: &'static str,
    pub category: Category,
    pub level: Level,
    pub flags: Vec<Flag>,
    /// `key=value` params this command's raw grammar accepts,
    /// each with its completion provider. Only meaningful for
    /// `raw_args` commands (the walker doesn't parse them; the
    /// completion tree advertises them as options AND completes
    /// their values).
    pub kv_params: &'static [KvParam],
    /// Context-sensitive extra option tokens (e.g. per-workload
    /// params discovered from the `workload=` already on the
    /// line). Wired to veks-completion's dynamic-options hook.
    pub dynamic_options: Option<DynamicOptions>,
    pub positionals: Vec<Positional>,
    pub subcommands: Vec<Command>,
    pub handler: Option<Handler>,
    /// When true, the walker stops parsing at this command's
    /// leaf and passes the remaining argv (verbatim) to the
    /// handler via [`ParsedCommand::raw`]. Use for commands
    /// whose argument grammar is too idiosyncratic for the
    /// generic walker (e.g. workload `key=value` params, the
    /// `report` vocab DSL). The flags declared in `flags`
    /// still drive completion — the spec stays the source of
    /// truth even when parsing is delegated.
    pub raw_args: bool,
    /// Optional override for the completion-tree node this
    /// command produces. When `Some`, the cli_spec→veks
    /// adapter calls this fn instead of synthesising a leaf
    /// from `flags`. Use for commands whose completion shape
    /// is structurally richer than a flat flag list (e.g. the
    /// SRD-64 vocab-driven kind subcommands that mix vocab
    /// directives with orthogonal dispatch flags and
    /// per-flag value providers).
    ///
    /// **Open gap:** this is an escape hatch — ideally the
    /// flag/value-provider model would be expressive enough
    /// that no override is needed. Future work: extend
    /// [`Flag`] / [`ValueProvider`] so vocab directives
    /// translate cleanly without escape.
    pub completion_override: Option<fn() -> veks_completion::Node>,
}

/// Category tag for the completion tree. Mirrors
/// [`crate::completion::Category`] one-to-one.
#[derive(Debug, Clone, Copy)]
pub enum Category {
    Workloads,
    Tools,
    Documentation,
    Benchmark,
    Server,
    Shell,
}

/// Tap-tier rank for the completion tree. Mirrors
/// [`crate::completion::Level`] one-to-one.
#[derive(Debug, Clone, Copy)]
pub enum Level {
    Workload,
    Secondary,
    FullSurface,
}

/// One flag declaration. Drives parsing, completion, validation,
/// and help-rendering uniformly.
pub struct Flag {
    pub long: &'static str,
    /// Optional short form (e.g. `-c` alongside `--command`).
    pub short: Option<&'static str>,
    /// Additional accepted long forms (e.g. `--to-file` as an
    /// alias for `--tofile`). Walker accepts any of these;
    /// completion advertises only `long`.
    pub aliases: &'static [&'static str],
    pub arity: Arity,
    pub value: ValueProvider,
    pub help: &'static str,
    /// True when the flag may appear more than once. The
    /// walker collects every occurrence into
    /// [`ParsedCommand::flags`]; non-repeatable flags only
    /// keep the last value.
    pub repeatable: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum Arity {
    /// Bare flag (`--foo`); presence means "true".
    Bool,
    /// `--foo bar` or `--foo=bar`.
    Value,
}

/// How the completion + help layers describe a flag's value.
/// Walker doesn't validate against this — handlers are
/// responsible for value validation. Completion uses it to
/// suggest candidates.
#[derive(Clone, Copy)]
/// A `key=value` parameter a command accepts in its raw argv
/// (the run-style grammar), with its tab-completion provider.
/// veks-completion 1.3.1 removed tree-global providers — value
/// completion is per-node, so each command declares the params
/// it actually understands and the spec stays canonical.
pub struct KvParam {
    /// Token prefix including the `=` (e.g. `"workload="`).
    pub key: &'static str,
    pub provider: fn(&str, &[&str]) -> Vec<String>,
}

pub enum ValueProvider {
    /// No specific suggestions; user types freely.
    None,
    /// Suggest filesystem paths (delegated to the shell's
    /// default filename completer for now — no custom logic).
    Path,
    /// Custom completer fn. Receives the partial value and the
    /// surrounding context tokens; returns matching candidates.
    Custom(fn(&str, &[&str]) -> Vec<String>),
}

pub struct Positional {
    pub name: &'static str,
    pub help: &'static str,
    pub kind: PositionalKind,
    /// Completion provider for this positional's value. Only
    /// the FIRST positional's provider is honored (veks-
    /// completion models one first-positional provider per
    /// node). `None`/`Path` behave as for flags.
    pub value: ValueProvider,
}

#[derive(Debug, Clone, Copy)]
pub enum PositionalKind {
    /// Required: walker errors if absent.
    One,
    /// Optional: zero or one accepted.
    ZeroOrOne,
    /// Zero or more — collects all remaining non-flag args.
    /// Reserved for variadic positional surfaces; no current
    /// command declares this, so the walker has no consumer yet.
    #[allow(dead_code)]
    Many,
}

/// Result of [`walker::parse`]. Handlers read flag values,
/// positionals, and (when `raw_args=true`) raw argv tail
/// from this struct.
#[derive(Debug, Clone)]
pub struct ParsedCommand {
    /// Path of matched commands, e.g. `["nmbrs", "metrics", "list"]`.
    pub path: Vec<String>,
    /// Flag values keyed by canonical (`--long`) name. Repeatable
    /// flags keep every occurrence in argv order; non-repeatable
    /// flags keep only the last (overwrite semantics).
    pub flags: BTreeMap<String, Vec<String>>,
    /// Bool flags that were set, keyed by canonical name.
    pub bools: BTreeSet<String>,
    /// Positional args in argv order.
    pub positionals: Vec<String>,
    /// When the matched leaf had `raw_args=true`, holds every
    /// argv token after the matched command path. Otherwise
    /// empty.
    pub raw: Vec<String>,
    /// Original argv (less the program name). Useful for
    /// raw-args handlers that delegate to legacy parsers.
    /// No current handler reads this; reserved for the legacy-
    /// delegation pattern.
    #[allow(dead_code)]
    pub argv: Vec<String>,
    /// True when `--help` or `-h` appeared anywhere in argv.
    /// The walker stops at the deepest subcommand seen *before*
    /// the help flag and returns immediately; main.rs renders
    /// usage for that command path instead of dispatching to
    /// the handler. Means handlers never have to handle help.
    pub help_requested: bool,
    /// True when `--version` or `-V` appeared anywhere in argv.
    /// Same short-circuit shape as [`help_requested`]: walker
    /// stops at the deepest subcommand seen before the flag and
    /// returns immediately; main.rs prints the version string
    /// and exits without dispatching.
    pub version_requested: bool,
}

impl ParsedCommand {
    /// First value for a flag, if present.
    pub fn flag(&self, name: &str) -> Option<&str> {
        self.flags
            .get(name)
            .and_then(|v| v.first())
            .map(|s| s.as_str())
    }
    /// All values for a (repeatable) flag, in argv order.
    pub fn flag_all(&self, name: &str) -> &[String] {
        self.flags.get(name).map(|v| v.as_slice()).unwrap_or(&[])
    }
    pub fn bool(&self, name: &str) -> bool {
        self.bools.contains(name)
    }
    pub fn positional(&self, idx: usize) -> Option<&str> {
        self.positionals.get(idx).map(|s| s.as_str())
    }
}

impl Category {
    pub fn tag(self) -> &'static str {
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

impl Level {
    pub fn rank(self) -> u32 {
        match self {
            Level::Workload => 1,
            Level::Secondary => 2,
            Level::FullSurface => 3,
        }
    }
}
