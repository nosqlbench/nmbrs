// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Stdout adapter: writes resolved ops to stdout or a file.
//!
//! Formats:
//!   stmt        — statement field only (default for `nmbrs run`)
//!   readout     — aligned name=value pairs, one per line per op
//!   assignments — compact name=value pairs on one line
//!   json        — JSON object per op (proper types, not all strings)
//!   jsonl       — same as json (alias for JSON Lines workflows)
//!   csv         — comma-separated values (optional header row)
//!   tsv         — tab-separated values
//!   raw         — values only, custom separator
//!
//! Options:
//!   filename=<path>    — output file (default: stdout)
//!   format=<name>      — output format (default: stmt)
//!   separator=<str>    — field separator for raw/csv formats
//!   header=true        — emit header row (csv/tsv formats)
//!   fields=a,b,c       — only include named fields
//!
//! Per-op-template channel routing (SRD-40b §9):
//!   stdout=terminal    — default, write rendered op to fd 1 / file
//!   stdout=eventlog    — emit through the runner's event log
//!                        (`nmbrs_runtime::diag!` Info level), suppressing
//!                        terminal/file output. Use when the op's
//!                        synthetic metric is the point and the rendered
//!                        line is just diagnostic.
//!   stdout=silent      — drop output entirely; op still executes.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nmbrs_runtime::adapter::{
    AdapterError, DriverAdapter, ExecutionError, OpDispenser, OpResult, ResolvedFields, TextBody,
};
use nmbrs_runtime::observer::LogLevel;
use nmbrs_runtime::wires::resolve_op_fields_via_wires;
use nmbrs_workload::model::ParsedOp;

/// Where the stdout adapter routes its rendered output for a
/// given op template (SRD-40b §9).
///
/// Selected per op via the `stdout:` op-template parameter.
/// Adapter-wide configuration (filename, format, etc.) is
/// independent — `Terminal` routes to whatever
/// [`OutputTarget`] the adapter was constructed with;
/// `EventLog` and `Silent` bypass it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StdoutChannel {
    /// Default: write rendered output to fd 1 / configured file.
    #[default]
    Terminal,
    /// Emit through the runner's event log (`crate::diag!`).
    /// Suppresses terminal/file output; the op still executes.
    EventLog,
    /// Drop output entirely. The op still executes and any
    /// synthetic metrics still record.
    Silent,
}

impl StdoutChannel {
    /// Parse a channel name. Accepted: `terminal` (default),
    /// `eventlog`, `silent`. Returns `Err` on any other value so
    /// the caller can fold the error into the workload diagnostic
    /// chain — silent acceptance would let typos disable output.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_lowercase().as_str() {
            "terminal" | "stdout" | "default" => Ok(Self::Terminal),
            "eventlog" | "log" | "diag" => Ok(Self::EventLog),
            "silent" | "drop" | "discard" | "none" => Ok(Self::Silent),
            other => Err(format!(
                "unknown stdout channel '{other}'. Available: terminal, eventlog, silent"
            )),
        }
    }
}

/// Output target for the stdout adapter.
enum OutputTarget {
    Stdout(BufWriter<io::Stdout>),
    File(BufWriter<File>),
}

impl Write for OutputTarget {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            OutputTarget::Stdout(w) => w.write(buf),
            OutputTarget::File(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            OutputTarget::Stdout(w) => w.flush(),
            OutputTarget::File(w) => w.flush(),
        }
    }
}

/// Configuration for the stdout adapter.
pub struct StdoutConfig {
    /// Output filename, or "stdout" for console output.
    pub filename: String,
    /// Whether to append a newline after each op.
    pub newline: bool,
    /// Output format.
    pub format: StdoutFormat,
    /// Optional field filter: only display these fields.
    /// Empty means display all fields.
    pub fields_filter: Vec<String>,
    /// Field separator for Raw format. Default: ","
    pub separator: String,
    /// Whether to emit a header row (for CSV/TSV/Raw).
    pub header: bool,
    /// Whether to process `{red}`, `{reset}`, etc. ANSI color tokens
    /// in rendered output. Enable with `--color` or `color=true`.
    pub color: bool,
}

impl Default for StdoutConfig {
    fn default() -> Self {
        Self {
            filename: "stdout".into(),
            newline: true,
            format: StdoutFormat::Assignments,
            fields_filter: Vec::new(),
            separator: ",".into(),
            header: false,
            color: false,
        }
    }
}

impl StdoutConfig {
    /// Construct a config from CLI/workload params.
    pub fn from_params(params: &std::collections::HashMap<String, String>) -> Self {
        let format = params
            .get("format")
            .map(|s| StdoutFormat::parse(s).unwrap_or(StdoutFormat::Assignments))
            .unwrap_or(StdoutFormat::Statement);
        Self {
            filename: params.get("filename").cloned().unwrap_or("stdout".into()),
            newline: true,
            format,
            fields_filter: Vec::new(),
            separator: params.get("separator").cloned().unwrap_or(",".into()),
            header: params
                .get("header")
                .map(|s| s == "true" || s == "1" || s == "yes")
                .unwrap_or(false),
            color: params
                .get("color")
                .map(|s| s == "true" || s == "1" || s == "yes")
                .unwrap_or(false),
        }
    }
}

/// How to render the resolved op fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StdoutFormat {
    /// The `stmt` field only. Default for `nmbrs run`.
    Statement,
    /// Aligned `name = value` pairs, one per line. Readable output
    /// for debugging multi-field ops.
    Readout,
    /// Compact `name=value, name=value` on one line.
    Assignments,
    /// JSON object with proper types (numbers stay numbers).
    Json,
    /// Comma-separated values. Use with `header=true` for column names.
    Csv,
    /// Tab-separated values. Use with `header=true` for column names.
    Tsv,
    /// Values only with a custom separator (`separator=|`).
    Raw,
}

/// Canonical format names accepted by [`StdoutFormat::parse`], in
/// declaration order. Single source for the parse error message AND
/// `format=` shell-completion suggestions, so the two never drift.
pub const FORMAT_NAMES: &[&str] = &[
    "stmt",
    "readout",
    "assignments",
    "json",
    "csv",
    "tsv",
    "raw",
];

impl StdoutFormat {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "statement" | "stmt" => Ok(Self::Statement),
            "readout" => Ok(Self::Readout),
            "assignments" | "assign" => Ok(Self::Assignments),
            "json" | "inlinejson" | "jsonl" => Ok(Self::Json),
            "csv" => Ok(Self::Csv),
            "tsv" => Ok(Self::Tsv),
            "raw" => Ok(Self::Raw),
            other => Err(format!(
                "unknown format: '{other}'. Available: {}",
                FORMAT_NAMES.join(", ")
            )),
        }
    }

    /// Render resolved fields to a string.
    pub fn render(&self, fields: &ResolvedFields, separator: &str) -> String {
        match self {
            Self::Statement => {
                // Statement mode: just the stmt field value, or all fields joined by newlines
                if let Some(stmt) = fields.get_str("stmt") {
                    stmt.to_string()
                } else if let Some(raw) = fields.get_str("raw") {
                    raw.to_string()
                } else if let Some(prepared) = fields.get_str("prepared") {
                    prepared.to_string()
                } else {
                    fields.strings().join("\n")
                }
            }
            Self::Readout => {
                // Aligned name = value, one per line
                let max_name_len = fields.names.iter().map(|n| n.len()).max().unwrap_or(0);
                fields
                    .names
                    .iter()
                    .zip(fields.strings().iter())
                    .map(|(k, v)| format!("  {:width$} = {v}", k, width = max_name_len))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Self::Assignments => fields
                .names
                .iter()
                .zip(fields.strings().iter())
                .map(|(k, v): (&String, &String)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", "),
            Self::Json => {
                // Proper JSON with typed values (numbers, bools, not all strings)
                let obj = fields.to_json();
                obj.to_string()
            }
            Self::Csv => {
                // CSV: quote fields that contain commas or quotes
                fields
                    .strings()
                    .iter()
                    .map(|v| csv_escape(v, ','))
                    .collect::<Vec<_>>()
                    .join(",")
            }
            Self::Tsv => {
                // TSV: escape tabs in values
                fields
                    .strings()
                    .iter()
                    .map(|v| v.replace('\t', "\\t"))
                    .collect::<Vec<_>>()
                    .join("\t")
            }
            Self::Raw => fields.strings().join(separator),
        }
    }

    /// Render a header row (field names) for tabular formats.
    fn render_header(&self, fields: &ResolvedFields, separator: &str) -> String {
        match self {
            Self::Csv => fields.names.join(","),
            Self::Tsv => fields.names.join("\t"),
            Self::Raw => fields.names.join(separator),
            _ => String::new(),
        }
    }

    /// Whether this format supports a header row.
    fn supports_header(&self) -> bool {
        matches!(self, Self::Csv | Self::Tsv | Self::Raw)
    }
}

/// Replace `{color}` tokens with ANSI escape sequences.
/// Supported: {red}, {green}, {yellow}, {blue}, {magenta}, {cyan},
/// {white}, {bold}, {dim}, {reset}, {underline}.
fn apply_ansi_colors(text: &str) -> String {
    text.replace("{red}", "\x1b[31m")
        .replace("{green}", "\x1b[32m")
        .replace("{yellow}", "\x1b[33m")
        .replace("{blue}", "\x1b[34m")
        .replace("{magenta}", "\x1b[35m")
        .replace("{cyan}", "\x1b[36m")
        .replace("{white}", "\x1b[37m")
        .replace("{bold}", "\x1b[1m")
        .replace("{dim}", "\x1b[2m")
        .replace("{underline}", "\x1b[4m")
        .replace("{reset}", "\x1b[0m")
}

/// Escape a value for CSV output (RFC 4180).
fn csv_escape(value: &str, delimiter: char) -> String {
    if value.contains(delimiter) || value.contains('"') || value.contains('\n') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// The stdout adapter: writes ops to stdout or a file.
pub struct StdoutAdapter {
    writer: Arc<Mutex<OutputTarget>>,
    newline: bool,
    format: StdoutFormat,
    config_fields_filter: Vec<String>,
    separator: String,
    header: bool,
    header_emitted: Arc<AtomicBool>,
    color: bool,
}

impl Default for StdoutAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl StdoutAdapter {
    /// Create with default config (stdout, newlines, assignments format).
    pub fn new() -> Self {
        Self::with_config(StdoutConfig::default())
    }

    /// Create with explicit config.
    pub fn with_config(config: StdoutConfig) -> Self {
        let writer = if config.filename.eq_ignore_ascii_case("stdout") {
            OutputTarget::Stdout(BufWriter::new(io::stdout()))
        } else {
            // Create parent directories on demand so `output=path/to/file.txt`
            // works without a manual `mkdir -p`. Bare filenames in the cwd
            // skip this since `parent()` returns `Some("")`.
            if let Some(parent) = std::path::Path::new(&config.filename).parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent).unwrap_or_else(|e| {
                    panic!(
                        "failed to create output directory '{}': {e}",
                        parent.display()
                    )
                });
            }
            let file = File::create(&config.filename).unwrap_or_else(|e| {
                panic!("failed to create output file '{}': {e}", config.filename)
            });
            OutputTarget::File(BufWriter::new(file))
        };
        Self {
            writer: Arc::new(Mutex::new(writer)),
            newline: config.newline,
            config_fields_filter: config.fields_filter,
            format: config.format,
            separator: config.separator,
            header: config.header,
            header_emitted: Arc::new(AtomicBool::new(false)),
            color: config.color,
        }
    }
}

impl DriverAdapter for StdoutAdapter {
    fn name(&self) -> &str {
        "stdout"
    }

    fn map_op<'a>(
        &'a self,
        template: &'a ParsedOp,
        parent: std::sync::Arc<dyn nmbrs_runtime::adapter::Kernel>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Box<dyn OpDispenser>, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            // SRD-40b §9: per-op-template channel routing. The
            // op-template parameter `stdout: <channel>` selects where
            // rendered output goes for this template. Absent → terminal.
            // Written on an op, the key stays among its op fields (the
            // parser moves only core keys into params); written in a
            // workload's `params:` block, it arrives in params.
            let channel = match template
                .op
                .get("stdout")
                .or_else(|| template.params.get("stdout"))
            {
                None => StdoutChannel::Terminal,
                Some(serde_json::Value::String(s)) => StdoutChannel::parse(s)
                    .map_err(|e| format!("op '{}' params.stdout: {e}", template.name))?,
                Some(other) => {
                    return Err(format!(
                        "op '{}' params.stdout must be a string (one of terminal/eventlog/silent), got {other}",
                        template.name
                    ));
                }
            };
            // SRD-68 Push 5: snapshot the op-field templates at
            // construction. At cycle time the dispenser walks this list
            // and resolves each field's `{name}` references through the
            // generic Polydat wires API (`wires.get` for pure-token positions,
            // `substitute_via_wires` for embedded references). No
            // synthesis-layer ResolvedFields needed — wires answers
            // every name directly.
            // The channel selector routes output; it isn't output.
            let op_fields: Vec<(String, serde_json::Value)> = template
                .op
                .iter()
                .filter(|(k, _)| k.as_str() != "stdout")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            Ok(Box::new(StdoutDispenser {
                writer: self.writer.clone(),
                format: self.format,
                newline: self.newline,
                fields_filter: self.config_fields_filter.clone(),
                separator: self.separator.clone(),
                header: self.header,
                header_emitted: self.header_emitted.clone(),
                color: self.color,
                channel,
                canonical_kernel: parent,
                op_fields,
            }) as Box<dyn OpDispenser>)
        })
    }

    // stdout is intentionally permissive: it renders whatever
    // op fields the workload supplies (stmt, method, url, etc.)
    // as data rather than dispatching on them. Returning `None`
    // opts out of SRD 30's strict unknown-field check, which
    // only makes sense for adapters that have a closed
    // vocabulary of known fields (CQL, HTTP when they're
    // refactored to declare their own). See SRD 30 §"Core-first
    // field processing" for the intended progression.

    /// Declare the SRD-40b §9 channel-routing op-template param
    /// so it survives the core's unknown-param guard.
    fn known_op_params(&self) -> &'static [&'static str] {
        &["stdout"]
    }
}

/// Op dispenser for the stdout adapter.
pub struct StdoutDispenser {
    writer: Arc<Mutex<OutputTarget>>,
    format: StdoutFormat,
    newline: bool,
    fields_filter: Vec<String>,
    separator: String,
    header: bool,
    header_emitted: Arc<AtomicBool>,
    color: bool,
    /// Per-op channel routing. SRD-40b §9.
    channel: StdoutChannel,
    /// SRD-68 invariant I-3: dispenser-owned canonical Polydat Kernel.
    /// Stored so the per-fiber fan-out can build per-fiber kernels
    /// from this dispenser's slot via the standard `build_subscope`
    /// path (see `OpDispenser::canonical_kernel`).
    canonical_kernel: std::sync::Arc<dyn nmbrs_runtime::adapter::Kernel>,
    /// Op-field templates snapshotted at `map_op` (name + raw
    /// JSON value from the parsed op). At cycle time each entry
    /// is resolved against the per-fiber wires: pure-token
    /// strings (`{name}`) preserve their typed `Value` via
    /// `wires.get`; embedded references (`{a}/{b}` etc.) render
    /// through `substitute_via_wires`. SRD-68 invariant I-1: the
    /// generic Polydat API answers every name; no synthesis-layer
    /// ResolvedFields is consulted.
    op_fields: Vec<(String, serde_json::Value)>,
}

impl OpDispenser for StdoutDispenser {
    fn canonical_kernel(&self) -> Option<&std::sync::Arc<dyn nmbrs_runtime::adapter::Kernel>> {
        Some(&self.canonical_kernel)
    }
    fn execute<'a>(
        &'a self,
        _cycle: u64,
        ctx: &'a nmbrs_runtime::adapter::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        let wires = ctx.wires;
        Box::pin(async move {
            // SRD-68 Push 5: resolve each op field through the
            // generic wires API. No ctx.fields lookup.
            let resolved = match resolve_op_fields_via_wires(&self.op_fields, wires) {
                Ok(r) => r,
                Err(msg) => {
                    return Err(ExecutionError::Op(AdapterError {
                        error_name: "BindError".into(),
                        message: msg,
                        retryable: false,
                    }));
                }
            };

            // Apply field filter if configured
            let filtered;
            let render_fields = if self.fields_filter.is_empty() {
                &resolved
            } else {
                let mut names: Vec<String> = Vec::new();
                let mut values = Vec::new();
                for (i, name) in resolved.names.iter().enumerate() {
                    if self.fields_filter.iter().any(|f| f == name) {
                        names.push(name.clone());
                        values.push(resolved.values[i].clone());
                    }
                }
                filtered = ResolvedFields::new(names, values);
                &filtered
            };

            let raw_text = self.format.render(render_fields, &self.separator);
            let text = if self.color {
                apply_ansi_colors(&raw_text)
            } else {
                raw_text
            };

            // SRD-40b §9 channel dispatch. Terminal keeps the
            // current write-to-OutputTarget path (with header
            // bookkeeping). EventLog routes the rendered line
            // through `nmbrs_runtime::diag!` so it lands in the
            // session log file and the runner observer (TUI ring
            // buffer / stderr) instead of the user-facing target.
            // Silent drops the line entirely; the op still
            // executes and any wrapping MetricsDispenser still
            // records.
            match self.channel {
                StdoutChannel::Terminal => {
                    // The real-stdout target staircases when a status display holds
                    // the terminal in raw mode, so route it through the observer's
                    // op-output channel (coordinated with the display when
                    // interactive; straight to stdout when piped/redirected, so
                    // `nmbrs run | grep` keeps working). A file target has no
                    // terminal — write to it directly.
                    let to_stdout = matches!(
                        &*self.writer.lock().unwrap_or_else(|e| e.into_inner()),
                        OutputTarget::Stdout(_)
                    );
                    if to_stdout {
                        if self.header
                            && self.format.supports_header()
                            && !self.header_emitted.swap(true, Ordering::Relaxed)
                        {
                            let header = self.format.render_header(render_fields, &self.separator);
                            if !header.is_empty() {
                                nmbrs_runtime::observer::op_output(&header);
                            }
                        }
                        nmbrs_runtime::observer::op_output(&text);
                    } else {
                        let result = {
                            let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
                            if self.header
                                && self.format.supports_header()
                                && !self.header_emitted.swap(true, Ordering::Relaxed)
                            {
                                let header =
                                    self.format.render_header(render_fields, &self.separator);
                                if !header.is_empty() {
                                    let _ = writeln!(writer, "{header}");
                                }
                            }
                            let write_result = if self.newline {
                                writeln!(writer, "{text}")
                            } else {
                                write!(writer, "{text}")
                            };
                            if let Err(e) = writer.flush() {
                                return Err(ExecutionError::Op(AdapterError {
                                    error_name: "FlushError".into(),
                                    message: e.to_string(),
                                    retryable: false,
                                }));
                            }
                            write_result
                        };
                        if let Err(e) = result {
                            return Err(ExecutionError::Op(AdapterError {
                                error_name: "IoError".into(),
                                message: e.to_string(),
                                retryable: false,
                            }));
                        }
                    }
                }
                StdoutChannel::EventLog => {
                    // Header row, when configured, also goes
                    // through the event log on its first emit.
                    // Stays consistent with the Terminal path's
                    // behaviour — operators that switch channels
                    // mid-development don't get surprised by
                    // header rows reappearing.
                    if self.header
                        && self.format.supports_header()
                        && !self.header_emitted.swap(true, Ordering::Relaxed)
                    {
                        let header = self.format.render_header(render_fields, &self.separator);
                        if !header.is_empty() {
                            nmbrs_runtime::diag!(LogLevel::Info, "{}", header);
                        }
                    }
                    nmbrs_runtime::diag!(LogLevel::Info, "{}", text);
                }
                StdoutChannel::Silent => {
                    // No emit. Op-execution side effects (running
                    // through the dispenser pipeline, populating
                    // Polydat wires, recording wrapped metrics) still
                    // happen by virtue of having reached this
                    // closure.
                }
            }

            Ok(OpResult {
                body: Some(Box::new(TextBody(text))),
                skipped: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_fields(fields: &[(&str, &str)]) -> ResolvedFields {
        ResolvedFields::new(
            fields.iter().map(|(k, _)| k.to_string()).collect(),
            fields
                .iter()
                .map(|(_, v)| polydat::ast::Value::Str((*v).into()))
                .collect(),
        )
    }

    /// Minimal kernel used as the `parent` argument to `map_op`
    /// in tests that don't need a richer Polydat context. SRD-68 Push 2:
    /// the `parent` parameter is plumbed through every `map_op`
    /// signature as `Arc<dyn Kernel>`; tests pass this fixture so
    /// they don't need to stand up the full activity-init pipeline.
    fn test_kernel() -> std::sync::Arc<dyn nmbrs_runtime::adapter::Kernel> {
        std::sync::Arc::new(
            polydat::dsl::compile::compile_polydat_interpreter("input cycle: u64\n").unwrap(),
        )
    }

    fn typed_fields() -> ResolvedFields {
        ResolvedFields::new(
            vec!["name".into(), "age".into(), "score".into()],
            vec![
                polydat::ast::Value::Str("alice".into()),
                polydat::ast::Value::U64(30),
                polydat::ast::Value::F64(3.5),
            ],
        )
    }

    #[test]
    fn format_statement_prefers_stmt_field() {
        let f = StdoutFormat::Statement;
        let fields = test_fields(&[("stmt", "SELECT 1"), ("other", "ignored")]);
        assert_eq!(f.render(&fields, ","), "SELECT 1");
    }

    #[test]
    fn format_statement_falls_back_to_raw() {
        let f = StdoutFormat::Statement;
        let fields = test_fields(&[("raw", "CREATE TABLE t")]);
        assert_eq!(f.render(&fields, ","), "CREATE TABLE t");
    }

    #[test]
    fn format_readout_aligned() {
        let f = StdoutFormat::Readout;
        let fields = test_fields(&[("name", "alice"), ("age", "30"), ("x", "1")]);
        let rendered = f.render(&fields, ",");
        // All '=' should be aligned
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("name = alice"));
        assert!(lines[1].contains("age  = 30"));
        assert!(lines[2].contains("x    = 1"));
    }

    #[test]
    fn format_assignments() {
        let f = StdoutFormat::Assignments;
        let fields = test_fields(&[("a", "1"), ("b", "2")]);
        assert_eq!(f.render(&fields, ","), "a=1, b=2");
    }

    #[test]
    fn format_json_typed_values() {
        let f = StdoutFormat::Json;
        let fields = typed_fields();
        let rendered = f.render(&fields, ",");
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["name"], "alice");
        assert_eq!(parsed["age"], 30);
        assert!((parsed["score"].as_f64().unwrap() - 3.5).abs() < 0.001);
    }

    #[test]
    fn format_csv_escapes_commas() {
        let f = StdoutFormat::Csv;
        let fields = test_fields(&[("a", "hello, world"), ("b", "plain")]);
        let rendered = f.render(&fields, ",");
        assert_eq!(rendered, "\"hello, world\",plain");
    }

    #[test]
    fn format_csv_header() {
        let f = StdoutFormat::Csv;
        let fields = test_fields(&[("name", "alice"), ("age", "30")]);
        let header = f.render_header(&fields, ",");
        assert_eq!(header, "name,age");
    }

    #[test]
    fn format_tsv() {
        let f = StdoutFormat::Tsv;
        let fields = test_fields(&[("a", "1"), ("b", "2")]);
        assert_eq!(f.render(&fields, "\t"), "1\t2");
    }

    #[test]
    fn format_raw_custom_separator() {
        let f = StdoutFormat::Raw;
        let fields = test_fields(&[("a", "1"), ("b", "2"), ("c", "3")]);
        assert_eq!(f.render(&fields, "|"), "1|2|3");
    }

    #[test]
    fn format_parse_all() {
        assert!(matches!(
            StdoutFormat::parse("stmt").unwrap(),
            StdoutFormat::Statement
        ));
        assert!(matches!(
            StdoutFormat::parse("readout").unwrap(),
            StdoutFormat::Readout
        ));
        assert!(matches!(
            StdoutFormat::parse("assignments").unwrap(),
            StdoutFormat::Assignments
        ));
        assert!(matches!(
            StdoutFormat::parse("json").unwrap(),
            StdoutFormat::Json
        ));
        assert!(matches!(
            StdoutFormat::parse("jsonl").unwrap(),
            StdoutFormat::Json
        ));
        assert!(matches!(
            StdoutFormat::parse("csv").unwrap(),
            StdoutFormat::Csv
        ));
        assert!(matches!(
            StdoutFormat::parse("tsv").unwrap(),
            StdoutFormat::Tsv
        ));
        assert!(matches!(
            StdoutFormat::parse("raw").unwrap(),
            StdoutFormat::Raw
        ));
        assert!(StdoutFormat::parse("bogus").is_err());
    }

    #[tokio::test]
    async fn dispenser_writes_to_file() {
        let path = std::env::temp_dir().join("nb_stdout_test.txt");
        let adapter = StdoutAdapter::with_config(StdoutConfig {
            filename: path.to_str().unwrap().into(),
            newline: true,
            format: StdoutFormat::Assignments,
            ..Default::default()
        });
        // Literal stmt — exercises the format renderer without
        // requiring a wires source for substitution.
        let template = ParsedOp::simple("test", "key=value42");
        let dispenser = adapter.map_op(&template, test_kernel()).await.unwrap();

        let mut k =
            polydat::dsl::compile::compile_polydat_interpreter("input cycle: u64\n").unwrap();
        let cw = nmbrs_runtime::wires::CycleWires::new(&mut k);
        let pulls = nmbrs_runtime::fixture::ResolvedPulls::empty();
        let empty = ResolvedFields::new(Vec::new(), Vec::new());
        let ctx = nmbrs_runtime::adapter::ExecCtx::with_wires(&empty, &pulls, &cw);
        dispenser.execute(0, &ctx).await.unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("key=value42"), "got: {content:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn dispenser_csv_with_header() {
        let path = std::env::temp_dir().join("nb_csv_header_test.txt");
        let adapter = StdoutAdapter::with_config(StdoutConfig {
            filename: path.to_str().unwrap().into(),
            newline: true,
            format: StdoutFormat::Csv,
            header: true,
            ..Default::default()
        });
        // Op template carries explicit `name` and `age` fields with
        // bind-point references; per-cycle values come from a GK
        // kernel whose `cycle` input drives the strings.
        let mut template = ParsedOp::simple("test", "test");
        template.op.remove("stmt");
        template
            .op
            .insert("name".into(), serde_json::Value::String("{name}".into()));
        template
            .op
            .insert("age".into(), serde_json::Value::String("{age}".into()));
        let dispenser = adapter.map_op(&template, test_kernel()).await.unwrap();

        // Two compiled kernels — one per row's wire values.
        let mut k1 = polydat::dsl::compile::compile_polydat_interpreter(
            "input cycle: u64\n\
             name := \"alice\"\n\
             age := \"30\"\n",
        )
        .unwrap();
        let mut k2 = polydat::dsl::compile::compile_polydat_interpreter(
            "input cycle: u64\n\
             name := \"bob\"\n\
             age := \"25\"\n",
        )
        .unwrap();
        let cw1 = nmbrs_runtime::wires::CycleWires::new(&mut k1);
        let cw2 = nmbrs_runtime::wires::CycleWires::new(&mut k2);
        let pulls = nmbrs_runtime::fixture::ResolvedPulls::empty();
        let empty = ResolvedFields::new(Vec::new(), Vec::new());
        let ctx1 = nmbrs_runtime::adapter::ExecCtx::with_wires(&empty, &pulls, &cw1);
        let ctx2 = nmbrs_runtime::adapter::ExecCtx::with_wires(&empty, &pulls, &cw2);
        dispenser.execute(0, &ctx1).await.unwrap();
        dispenser.execute(1, &ctx2).await.unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        // CSV header pairs match HashMap iteration order; assert
        // membership rather than position so the test isn't tied
        // to non-deterministic op-field ordering.
        assert!(
            lines[0].contains("name") && lines[0].contains("age"),
            "header row should list both fields: {:?}",
            lines[0]
        );
        assert!(
            content.contains("alice") && content.contains("30"),
            "row 1 should render name=alice, age=30: {content:?}"
        );
        assert!(
            content.contains("bob") && content.contains("25"),
            "row 2 should render name=bob, age=25: {content:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    // -------------------------------------------------------------
    // SRD-40b §9 channel-routing tests
    // -------------------------------------------------------------

    /// Process-wide capturing observer for `eventlog` channel
    /// tests. Installed once via `set_global_observer` (an
    /// internal `OnceLock`); subsequent test runs reuse it.
    /// Other test processes don't share this observer.
    use std::sync::{Mutex as StdMutex, OnceLock as StdOnceLock};

    /// Shared sink of `(level, message)` pairs captured from the
    /// process-wide observer used by the channel-routing tests.
    type CapturedLogs = Arc<StdMutex<Vec<(LogLevel, String)>>>;

    static CAPTURED_LOGS: StdOnceLock<CapturedLogs> = StdOnceLock::new();

    fn captured_logs() -> &'static CapturedLogs {
        CAPTURED_LOGS.get_or_init(|| Arc::new(StdMutex::new(Vec::new())))
    }

    struct CapturingObserver {
        sink: CapturedLogs,
    }

    impl nmbrs_runtime::observer::RunObserver for CapturingObserver {
        fn phase_starting(
            &self,
            _: nmbrs_runtime::scene_tree::SceneNodeId,
            _: &str,
            _: &str,
            _: usize,
            _: u64,
            _: usize,
        ) {
        }
        fn phase_completed(
            &self,
            _: nmbrs_runtime::scene_tree::SceneNodeId,
            _: &str,
            _: &str,
            _: f64,
        ) {
        }
        fn phase_failed(
            &self,
            _: nmbrs_runtime::scene_tree::SceneNodeId,
            _: &str,
            _: &str,
            _: &str,
        ) {
        }
        fn phase_progress(&self, _: &nmbrs_runtime::observer::PhaseProgressUpdate) {}
        fn run_finished(&self) {}
        fn log(&self, level: LogLevel, message: &str) {
            self.sink.lock().unwrap().push((level, message.to_string()));
        }
    }

    /// Install the capturing observer exactly once for this test
    /// process. The runtime's `GLOBAL_OBSERVER` is a `OnceLock`
    /// that silently no-ops on a second `set` — the first call
    /// wins. As long as every channel-routing test takes this
    /// path, they share one observer and the captured-logs vec.
    fn install_capturing_observer() -> CapturedLogs {
        let sink = captured_logs().clone();
        let observer: Arc<dyn nmbrs_runtime::observer::RunObserver> =
            Arc::new(CapturingObserver { sink: sink.clone() });
        nmbrs_runtime::observer::set_global_observer(observer);
        sink
    }

    #[test]
    fn channel_parse_accepts_documented_aliases() {
        assert_eq!(
            StdoutChannel::parse("terminal").unwrap(),
            StdoutChannel::Terminal
        );
        assert_eq!(
            StdoutChannel::parse("eventlog").unwrap(),
            StdoutChannel::EventLog
        );
        assert_eq!(
            StdoutChannel::parse("silent").unwrap(),
            StdoutChannel::Silent
        );
        // Aliases — lets workloads reach for the natural word.
        assert_eq!(
            StdoutChannel::parse("LOG").unwrap(),
            StdoutChannel::EventLog
        );
        assert_eq!(
            StdoutChannel::parse(" Drop ").unwrap(),
            StdoutChannel::Silent
        );
        assert_eq!(
            StdoutChannel::parse("default").unwrap(),
            StdoutChannel::Terminal
        );
        // Typo is rejected, not silently treated as terminal.
        assert!(StdoutChannel::parse("eventlogg").is_err());
        assert!(StdoutChannel::parse("").is_err());
    }

    #[tokio::test]
    async fn terminal_is_default_when_param_absent() {
        // Sanity: omitting `params.stdout` keeps the legacy
        // behavior — output flows to the configured target.
        let path = std::env::temp_dir().join("nb_chan_default.txt");
        let adapter = StdoutAdapter::with_config(StdoutConfig {
            filename: path.to_str().unwrap().into(),
            newline: true,
            format: StdoutFormat::Assignments,
            ..Default::default()
        });
        let template = ParsedOp::simple("t", "stmt");
        // No `params.stdout` set.
        assert!(
            !template.params.contains_key("stdout"),
            "guard: this test relies on the param being absent"
        );

        // Re-bind the template stmt to the marker so the rendered
        // line carries it (op-field text is the source of truth).
        let mut template = template;
        template.op.insert(
            "stmt".into(),
            serde_json::Value::String("default_terminal_marker_abc".into()),
        );
        let dispenser = adapter.map_op(&template, test_kernel()).await.unwrap();
        let mut k =
            polydat::dsl::compile::compile_polydat_interpreter("input cycle: u64\n").unwrap();
        let cw = nmbrs_runtime::wires::CycleWires::new(&mut k);
        let pulls = nmbrs_runtime::fixture::ResolvedPulls::empty();
        let empty = ResolvedFields::new(Vec::new(), Vec::new());
        let ctx = nmbrs_runtime::adapter::ExecCtx::with_wires(&empty, &pulls, &cw);
        dispenser.execute(0, &ctx).await.unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("default_terminal_marker_abc"),
            "Terminal default should write to the file target; got: {content:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn eventlog_channel_routes_to_observer_log_not_file() {
        // The captured-logs sink is process-wide (OnceLock) and
        // shared across every channel-routing test in this
        // module — see `install_capturing_observer`. Don't
        // `clear()` it: under cargo's parallel test runner that
        // race-wipes a sibling test's marker between its push
        // and its read. Each test's marker is unique, so we
        // just search for ours and ignore anything else present.
        let sink = install_capturing_observer();

        // Configure the adapter to write to a temp file so we
        // can assert the file is *not* touched when the channel
        // is `eventlog`.
        let path = std::env::temp_dir().join("nb_eventlog_test.txt");
        // Pre-create the file empty so we can detect a write
        // unambiguously: any non-empty content = the dispenser
        // wrote. (`File::create` truncates on adapter
        // construction, so this also resets prior runs.)
        let adapter = StdoutAdapter::with_config(StdoutConfig {
            filename: path.to_str().unwrap().into(),
            newline: true,
            format: StdoutFormat::Assignments,
            ..Default::default()
        });

        // Op template carries the marker as literal stmt text and
        // requests the eventlog channel.
        let mut template = ParsedOp::simple("eventlog_op", "v_routed_to_eventlog_xyz123");
        template.params.insert(
            "stdout".into(),
            serde_json::Value::String("eventlog".into()),
        );

        let dispenser = adapter.map_op(&template, test_kernel()).await.unwrap();
        let mut k =
            polydat::dsl::compile::compile_polydat_interpreter("input cycle: u64\n").unwrap();
        let cw = nmbrs_runtime::wires::CycleWires::new(&mut k);
        let pulls = nmbrs_runtime::fixture::ResolvedPulls::empty();
        let empty = ResolvedFields::new(Vec::new(), Vec::new());
        let ctx = nmbrs_runtime::adapter::ExecCtx::with_wires(&empty, &pulls, &cw);
        let result = dispenser.execute(0, &ctx).await.unwrap();

        // The OpResult body still carries the rendered text so
        // capture-extraction still works for synthetic-metric
        // wrappers.
        assert!(!result.skipped);

        // The file must remain empty — no Terminal write.
        let file_contents = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            file_contents.is_empty(),
            "eventlog channel must not write to the file target; \
             got contents: {file_contents:?}"
        );
        let _ = std::fs::remove_file(&path);

        // The capturing observer must have received the rendered
        // line at Info level.
        let logs = sink.lock().unwrap().clone();
        let matched = logs.iter().any(|(lvl, msg)| {
            *lvl == LogLevel::Info && msg.contains("v_routed_to_eventlog_xyz123")
        });
        assert!(
            matched,
            "expected eventlog channel to emit through the observer log; \
             captured: {logs:?}"
        );
    }

    /// `stdout: <channel>` written on an op in workload YAML, as users
    /// write it. The parser leaves the key among the op fields (it moves
    /// only core keys into params), so the channel must be read there.
    #[tokio::test]
    async fn op_level_channel_from_yaml_is_honored() {
        let sink = install_capturing_observer();
        let yaml =
            "ops:\n  quiet:\n    stdout: silent\n    stmt: must_not_appear_yaml_marker_zx41\n";
        let workload = nmbrs_workload::parse::parse_workload(yaml, &Default::default()).unwrap();
        let template = &workload.ops[0];

        let path = std::env::temp_dir().join("nb_yaml_channel_test.txt");
        let adapter = StdoutAdapter::with_config(StdoutConfig {
            filename: path.to_str().unwrap().into(),
            format: StdoutFormat::Readout,
            ..Default::default()
        });
        let dispenser = adapter.map_op(template, test_kernel()).await.unwrap();
        let mut k =
            polydat::dsl::compile::compile_polydat_interpreter("input cycle: u64\n").unwrap();
        let cw = nmbrs_runtime::wires::CycleWires::new(&mut k);
        let pulls = nmbrs_runtime::fixture::ResolvedPulls::empty();
        let empty = ResolvedFields::new(Vec::new(), Vec::new());
        let ctx = nmbrs_runtime::adapter::ExecCtx::with_wires(&empty, &pulls, &cw);
        dispenser.execute(0, &ctx).await.unwrap();

        let file_contents = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        assert!(
            file_contents.is_empty(),
            "an op-level `stdout: silent` must not write: {file_contents:?}"
        );
        let logs = sink.lock().unwrap().clone();
        assert!(
            !logs
                .iter()
                .any(|(_, msg)| msg.contains("must_not_appear_yaml_marker_zx41")),
            "an op-level `stdout: silent` must not emit through the observer; logs: {logs:?}"
        );
    }

    /// The channel selector routes the output; it is not a field of it.
    #[tokio::test]
    async fn channel_selector_is_not_rendered() {
        let yaml = "ops:\n  shown:\n    stdout: terminal\n    stmt: visible_marker_kq27\n";
        let workload = nmbrs_workload::parse::parse_workload(yaml, &Default::default()).unwrap();
        let path = std::env::temp_dir().join("nb_channel_selector_test.txt");
        let adapter = StdoutAdapter::with_config(StdoutConfig {
            filename: path.to_str().unwrap().into(),
            format: StdoutFormat::Readout,
            ..Default::default()
        });
        let dispenser = adapter
            .map_op(&workload.ops[0], test_kernel())
            .await
            .unwrap();
        let mut k =
            polydat::dsl::compile::compile_polydat_interpreter("input cycle: u64\n").unwrap();
        let cw = nmbrs_runtime::wires::CycleWires::new(&mut k);
        let pulls = nmbrs_runtime::fixture::ResolvedPulls::empty();
        let empty = ResolvedFields::new(Vec::new(), Vec::new());
        let ctx = nmbrs_runtime::adapter::ExecCtx::with_wires(&empty, &pulls, &cw);
        dispenser.execute(0, &ctx).await.unwrap();

        let written = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        assert!(written.contains("visible_marker_kq27"), "{written:?}");
        assert!(
            !written.contains("stdout"),
            "selector rendered: {written:?}"
        );
    }

    #[tokio::test]
    async fn silent_channel_writes_nothing_anywhere() {
        // Don't `clear()` the shared sink — see the parallel
        // sibling test for the reason. Marker matching is
        // unique-per-test.
        let sink = install_capturing_observer();

        let path = std::env::temp_dir().join("nb_silent_test.txt");
        let adapter = StdoutAdapter::with_config(StdoutConfig {
            filename: path.to_str().unwrap().into(),
            ..Default::default()
        });
        let mut template = ParsedOp::simple("silent_op", "must_not_appear_unique_marker_pq987");
        template
            .params
            .insert("stdout".into(), serde_json::Value::String("silent".into()));

        let dispenser = adapter.map_op(&template, test_kernel()).await.unwrap();
        let mut k =
            polydat::dsl::compile::compile_polydat_interpreter("input cycle: u64\n").unwrap();
        let cw = nmbrs_runtime::wires::CycleWires::new(&mut k);
        let pulls = nmbrs_runtime::fixture::ResolvedPulls::empty();
        let empty = ResolvedFields::new(Vec::new(), Vec::new());
        let ctx = nmbrs_runtime::adapter::ExecCtx::with_wires(&empty, &pulls, &cw);
        dispenser.execute(0, &ctx).await.unwrap();

        let file_contents = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            file_contents.is_empty(),
            "silent channel must not write to file: {file_contents:?}"
        );
        let _ = std::fs::remove_file(&path);

        let logs = sink.lock().unwrap().clone();
        let leaked = logs
            .iter()
            .any(|(_, msg)| msg.contains("must_not_appear_unique_marker_pq987"));
        assert!(
            !leaked,
            "silent channel must not emit through observer; logs: {logs:?}"
        );
    }

    #[tokio::test]
    async fn map_op_rejects_unknown_channel_value() {
        let adapter = StdoutAdapter::new();
        let mut template = ParsedOp::simple("bad", "ignored");
        template
            .params
            .insert("stdout".into(), serde_json::Value::String("nope".into()));
        let err = match adapter.map_op(&template, test_kernel()).await {
            Ok(_) => panic!("unknown channel must error"),
            Err(e) => e,
        };
        assert!(
            err.contains("unknown stdout channel"),
            "diagnostic should explain: {err}"
        );
        assert!(
            err.contains("'bad'"),
            "diagnostic should name the op: {err}"
        );
    }

    #[tokio::test]
    async fn map_op_rejects_non_string_channel_value() {
        let adapter = StdoutAdapter::new();
        let mut template = ParsedOp::simple("bad", "ignored");
        template
            .params
            .insert("stdout".into(), serde_json::Value::Bool(true));
        let err = match adapter.map_op(&template, test_kernel()).await {
            Ok(_) => panic!("non-string channel must error"),
            Err(e) => e,
        };
        assert!(err.contains("must be a string"), "got: {err}");
    }
}

// =========================================================================
// Adapter Registration (inventory-based, link-time)
// =========================================================================

inventory::submit! {
    nmbrs_runtime::adapter::AdapterRegistration {
        names: || &["stdout"],
        known_params: || &["filename", "format", "separator", "header", "color", "fields"],
        display_preference: |params| {
            // Writing op output to the console conflicts with the TUI
            // (the dashboard would mangle/staircase the raw lines), so
            // prefer tui=off there and let the log sink capture the run
            // detail. When `filename=` redirects output to a file, the
            // console is free and the adapter is TUI-compatible.
            let to_console = params.get("filename")
                .map(|f| f.eq_ignore_ascii_case("stdout"))
                .unwrap_or(true);
            if to_console {
                nmbrs_runtime::adapter::DisplayPreference::Off
            } else {
                nmbrs_runtime::adapter::DisplayPreference::Auto
            }
        },
        supported_controls: || &[],
        create: |params| Box::pin(async move {
            // Reject an unknown `format=` rather than silently falling
            // back to the default (`feedback_never_ignore_silently`).
            if let Some(f) = params.get("format") {
                StdoutFormat::parse(f)?;
            }
            Ok(std::sync::Arc::new(StdoutAdapter::with_config(StdoutConfig::from_params(&params)))
                as std::sync::Arc<dyn nmbrs_runtime::adapter::DriverAdapter>)
        }),
    }
}

// SRD-35 Push C: stdout adapter declares itself
// pool-shareable. `println!` / `write!` are
// thread-safe; the underlying `StdoutAdapter` holds
// configuration but no per-phase mutable state that
// would forbid sharing. Phases targeting the same
// output sink (filename + format + separator) share one
// adapter, avoiding per-phase file-handle re-open
// churn.
inventory::submit! {
    nmbrs_runtime::adapter::SharedDriverRegistration {
        adapter: "stdout",
        driver: nmbrs_runtime::adapter::DEFAULT_DRIVER_NAME,
        share_capability: nmbrs_runtime::resource_pool::ShareCapability::Shared,
        resource_key: |params| {
            // Identity-bearing: the output destination
            // and on-write formatting decisions. Per-op
            // `fields` values come from op templates and
            // don't shape the underlying writer.
            let mut k = nmbrs_runtime::resource_pool::ResourceKey::new("stdout");
            for field in ["filename", "format", "separator", "header", "color"] {
                if let Some(v) = params.get(field) {
                    k = k.with(field, v.clone());
                }
            }
            Ok(k)
        },
    }
}
