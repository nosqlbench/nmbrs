// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! OpenAPI subcommand glue (`describe` / `run` from spec).
//!
//! Compiled only when the `openapi` Cargo feature is enabled,
//! which pulls in [`nmbrs_adapter_openapi`] for spec parsing and
//! workload synthesis. This module owns:
//!
//! - `nmbrs describe-openapi spec=<file>` — spec inspection
//!   (operations, tag summary).
//! - `nmbrs run-openapi spec=<file> [base_url=...] [adapter=...]
//!   [...]` — synthesizes ops + Polydat bindings from the spec and
//!   runs them against the chosen adapter (default: `http`).
//!
//! Pairs with [`nmbrs_adapter_openapi`], which carries the spec
//! parser + workload synthesis. nmbrs is the single user-facing
//! CLI; opting in via `--features openapi` adds these
//! subcommands without affecting the default build.

use std::collections::HashMap;
use std::sync::Arc;

use nmbrs_adapter_openapi::{ApiOperation, describe_operations, generate_ops, parse_spec};
use nmbrs_metrics::labels::Labels;
use nmbrs_runtime::activity::{Activity, ActivityConfig};
use nmbrs_runtime::adapter::DriverAdapter;
use nmbrs_runtime::bindings::compile_bindings;
use nmbrs_runtime::opseq::{OpSequence, SequencerType};
use nmbrs_runtime::synthesis::OpBuilder;

fn parse_params(args: &[String]) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for arg in args {
        if arg.starts_with("--") || arg.starts_with('-') {
            continue;
        }
        if let Some(eq_pos) = arg.find('=') {
            let key = arg[..eq_pos].to_string();
            let val = arg[eq_pos + 1..].to_string();
            params.insert(key, val);
        }
    }
    params
}

fn load_spec(
    params: &HashMap<String, String>,
) -> Result<(openapiv3::OpenAPI, Vec<ApiOperation>), String> {
    let spec_path = params
        .get("spec")
        .ok_or("missing required parameter: spec=<file.yaml>")?;
    let source = std::fs::read_to_string(spec_path)
        .map_err(|e| format!("failed to read spec '{spec_path}': {e}"))?;
    parse_spec(&source)
}

/// `nmbrs describe-openapi spec=<file>` — inspect operations and
/// tag summary without running anything.
pub fn describe_command(args: &[String]) {
    let params = parse_params(args);
    let (api, ops) = match load_spec(&params) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let title = api.info.title.as_str();
    let version = &api.info.version;
    println!("OpenAPI: {title} v{version}");
    println!("Operations ({}):", ops.len());
    describe_operations(&ops);

    let mut tag_counts: HashMap<&str, usize> = HashMap::new();
    for op in &ops {
        for tag in &op.tags {
            *tag_counts.entry(tag.as_str()).or_insert(0) += 1;
        }
    }
    if !tag_counts.is_empty() {
        println!("\nTags:");
        for (tag, count) in &tag_counts {
            println!("  {tag}: {count} operations");
        }
    }
}

/// `nmbrs run-openapi spec=<file> ...` — synthesize ops from the
/// spec and run them through the chosen adapter.
pub async fn run_command(args: &[String]) {
    let params = parse_params(args);

    let (_api, api_ops) = match load_spec(&params) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    if api_ops.is_empty() {
        eprintln!("error: no operations found in spec");
        std::process::exit(1);
    }

    let ops_to_run: Vec<ApiOperation> = if let Some(filter) = params.get("operations") {
        let ids: Vec<&str> = filter.split(',').map(|s| s.trim()).collect();
        api_ops
            .into_iter()
            .filter(|op| ids.contains(&op.operation_id.as_str()))
            .collect()
    } else {
        api_ops
    };

    if ops_to_run.is_empty() {
        eprintln!("error: no operations match the filter");
        std::process::exit(1);
    }

    let base_url = params
        .get("base_url")
        .or_else(|| params.get("host"))
        .cloned()
        .unwrap_or_else(|| "http://localhost:8080".into());

    let (parsed_ops, bindings_source) = generate_ops(&ops_to_run, &base_url);

    eprintln!(
        "openapi: {} operations, base_url={}",
        parsed_ops.len(),
        base_url
    );
    for op in &parsed_ops {
        eprintln!(
            "  {} {} {}",
            op.op.get("method").and_then(|v| v.as_str()).unwrap_or("?"),
            op.op.get("uri").and_then(|v| v.as_str()).unwrap_or("?"),
            op.name
        );
    }

    if !bindings_source.is_empty() {
        eprintln!("openapi: Polydat bindings:\n{bindings_source}");
    }

    let kernel = match compile_bindings(&parsed_ops) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("error: failed to compile bindings: {e}");
            std::process::exit(1);
        }
    };

    let explicit_cycles: Option<u64> = params.get("cycles").and_then(|s| s.parse().ok());
    let threads: usize = params
        .get("threads")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let seq_type = params
        .get("seq")
        .map(|s| SequencerType::parse(s).unwrap_or(SequencerType::Bucket))
        .unwrap_or(SequencerType::Bucket);

    let op_sequence = OpSequence::from_ops(parsed_ops, seq_type);
    let cycles = explicit_cycles.unwrap_or(op_sequence.stanza_length() as u64);

    let config = ActivityConfig {
        name: "openapi".into(),
        cycles,
        concurrency: threads,
        rate: params.get("rate").and_then(|s| s.parse().ok()),
        sequencer: seq_type,
        error_spec: params.get("errors").cloned().unwrap_or_default(),
        max_retries: 3,
        stanza_concurrency: params
            .get("stanza_concurrency")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1),
        ..Default::default()
    };

    let builder = Arc::new(OpBuilder::new(kernel));
    let activity = Activity::new(config, &Labels::of("session", "openapi"), op_sequence);

    eprintln!("openapi: {cycles} cycles, {threads} threads");

    let driver = params
        .get("adapter")
        .or(params.get("driver"))
        .map(|s| s.as_str())
        .unwrap_or("http");

    let adapter: Arc<dyn DriverAdapter> = match driver {
        "http" => {
            use nmbrs_adapter_http::{HttpAdapter, HttpConfig};
            Arc::new(HttpAdapter::with_config(HttpConfig {
                base_url: None, // base_url already in URI templates
                timeout_ms: params
                    .get("timeout")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(30_000),
                follow_redirects: true,
            }))
        }
        "stdout" => {
            use nmbrs_adapter_stdout::{StdoutAdapter, StdoutConfig, StdoutFormat};
            let format = match params.get("format").map(|s| s.as_str()) {
                Some("json") => StdoutFormat::Json,
                Some("csv") => StdoutFormat::Csv,
                _ => StdoutFormat::Assignments,
            };
            Arc::new(StdoutAdapter::with_config(StdoutConfig {
                filename: params.get("filename").cloned().unwrap_or("stdout".into()),
                format,
                ..Default::default()
            }))
        }
        "testkit" => {
            use nmbrs_adapter_stdout::{StdoutConfig, StdoutFormat};
            use nmbrs_adapter_testkit::{ModelAdapter, ModelConfig};
            Arc::new(ModelAdapter::with_config(ModelConfig {
                stdout: StdoutConfig {
                    format: StdoutFormat::Assignments,
                    ..Default::default()
                },
                diagnose: args.iter().any(|a| a == "--diagnose"),
            }))
        }
        other => {
            eprintln!("error: unknown adapter '{other}' (available: http, stdout, testkit)");
            std::process::exit(1);
        }
    };

    activity.run_with_driver(adapter, builder).await;
    eprintln!("openapi: done.");
}

/// CLI spec wrapper for `nmbrs describe-openapi`. Bridges the
/// historical `describe_command(args)` arg-style entry point
/// into the [`crate::cli_spec::Command`] surface used by the
/// rest of the CLI (see `cli_spec/root.rs`).
/// `spec=` names the OpenAPI document on disk.
/// `spec=<file>` — defined once for both openapi commands.
const SPEC_KV_PARAM: crate::cli_spec::KvParam = crate::cli_spec::KvParam {
    key: "spec=",
    provider: crate::completion::spec_file_provider,
};

static OPENAPI_DESCRIBE_KV: &[crate::cli_spec::KvParam] = &[SPEC_KV_PARAM];

static OPENAPI_RUN_KV: &[crate::cli_spec::KvParam] = &[
    SPEC_KV_PARAM,
    crate::cli_spec::KvParam {
        key: "adapter=",
        provider: crate::completion::adapter_names_provider,
    },
    // The rest of the surface run_openapi_command actually reads
    // (openapi.rs parse sites) — registered so completion/help see it.
    crate::cli_spec::KvParam {
        key: "operations=",
        provider: crate::completion::free_form,
    },
    crate::cli_spec::KvParam {
        key: "base_url=",
        provider: crate::completion::free_form,
    },
    crate::cli_spec::KvParam {
        key: "host=",
        provider: crate::completion::free_form,
    },
    crate::completion::CYCLES_KV_PARAM,
    crate::completion::THREADS_KV_PARAM,
    crate::completion::SEQ_KV_PARAM,
    crate::completion::RATE_KV_PARAM,
    crate::completion::ERRORS_KV_PARAM,
    crate::completion::STANZA_CONCURRENCY_KV_PARAM,
    crate::completion::DRIVER_KV_PARAM,
    crate::cli_spec::KvParam {
        key: "timeout=",
        provider: crate::completion::free_form,
    },
    crate::cli_spec::KvParam {
        key: "format=",
        provider: crate::completion::free_form,
    },
    crate::completion::FILENAME_KV_PARAM,
];

pub fn describe_spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle(p: ParsedCommand) -> Result<(), String> {
        describe_command(&p.raw);
        Ok(())
    }
    Command {
        name: "describe-openapi",
        help: "Inspect an OpenAPI spec: operations and tag summary.",
        category: Category::Documentation,
        level: Level::FullSurface,
        flags: Vec::new(),
        kv_params: OPENAPI_DESCRIBE_KV,
        dynamic_options: None,
        positionals: Vec::new(),
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: true,
        completion_override: None,
    }
}

/// CLI spec wrapper for `nmbrs run-openapi`. Same bridging
/// pattern as [`describe_spec`].
pub fn run_spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle(p: ParsedCommand) -> Result<(), String> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| format!("openapi: failed to create tokio runtime: {e}"))?;
        rt.block_on(run_command(&p.raw));
        Ok(())
    }
    Command {
        name: "run-openapi",
        help: "Synthesize ops from an OpenAPI spec and run against the chosen adapter.",
        category: Category::Documentation,
        level: Level::FullSurface,
        flags: Vec::new(),
        kv_params: OPENAPI_RUN_KV,
        dynamic_options: None,
        positionals: Vec::new(),
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: true,
        completion_override: None,
    }
}
