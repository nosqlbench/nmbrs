// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `openapi-ops` — render an OpenAPI 3.x spec as a library of reusable
//! http op templates (see `nmbrs_adapter_openapi::library`).
//!
//! ```text
//! openapi-ops <spec.yaml|json> [<out.yaml>] [base_url=<url>]
//! ```
//!
//! With no output file the library goes to stdout. An existing output
//! file is never overwritten.

use std::process::ExitCode;

const USAGE: &str = "usage: openapi-ops <spec.yaml|json> [<out.yaml>] [base_url=<url>]

Renders every operation in an OpenAPI 3.x spec as a reusable http op
template. A workload `extends:` the output and instantiates operations
with `uses: <operationId>`, supplying the fields each one needs.";

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("openapi-ops: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let mut base_url: Option<String> = None;
    let mut paths: Vec<&str> = Vec::new();
    for arg in &args {
        match arg.split_once('=') {
            Some(("base_url", url)) => base_url = Some(url.to_string()),
            Some((key, _)) => return Err(format!("unknown option '{key}='\n\n{USAGE}")),
            None => paths.push(arg),
        }
    }
    let (spec_path, out_path) = match paths.as_slice() {
        [spec] => (*spec, None),
        [spec, out] => (*spec, Some(*out)),
        _ => return Err(USAGE.to_string()),
    };

    let source =
        std::fs::read_to_string(spec_path).map_err(|e| format!("read {spec_path}: {e}"))?;
    let (api, ops) = nmbrs_adapter_openapi::parse_spec(&source)?;
    if ops.is_empty() {
        return Err(format!("{spec_path} declares no operations"));
    }
    let spec_name = std::path::Path::new(spec_path)
        .file_name()
        .map_or(spec_path.to_string(), |n| n.to_string_lossy().into_owned());
    let library =
        nmbrs_adapter_openapi::render_op_library(&api, &ops, &spec_name, base_url.as_deref());

    match out_path {
        None => print!("{library}"),
        Some(out) => {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(out)
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::AlreadyExists => {
                        format!("{out} exists; not overwriting it")
                    }
                    _ => format!("write {out}: {e}"),
                })?;
            std::io::Write::write_all(&mut file, library.as_bytes())
                .map_err(|e| format!("write {out}: {e}"))?;
            eprintln!("openapi-ops: {} op templates -> {out}", ops.len());
        }
    }
    Ok(())
}
