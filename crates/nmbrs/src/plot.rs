// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The `wiring visualize` subcommand.
//!
//! This is **sugar** over `nmbrs run adapter=plotter render=single`: it
//! builds a one-op plotter workload from the given expression (or
//! `.polydat` file) and runs it through the activity engine. All
//! rendering lives in the plotter adapter (`crates/nmbrs-adapter-plotter`) — there
//! is no parallel renderer here. The adapter chooses the plot mode from
//! the output wire names (`x`/`y` → parametric, `r`/`theta` → polar,
//! otherwise a line plot); `--mode` overrides it.
//!
//! Because it's a real engine run, the live-vs-single distinction is the
//! adapter's `render=` knob: `wiring visualize` always asks for
//! `render=single` (one static snapshot), whereas
//! `nmbrs run adapter=plotter …` defaults to `render=live` (animated).

/// Translate `wiring visualize <expr|file> [opts]` into the equivalent
/// `nmbrs run adapter=plotter render=single …` argv.
///
/// Option mapping:
/// - `<expr>` / `<file.polydat>` → `op=<source>`
/// - `cycles=N` → `cycles=N` (default 1000)
/// - `--mode=M` → `mode=M` (`auto`/`plot`/`parametric`/`xy`/`polar`)
/// - `--width=W` / `--height=H` → `width=`/`height=`
/// - `output=a,b` → `lanes=a;b` (each field its own band)
/// - `--no-color` → `no_color=true`
fn build_run_args(raw: &[String]) -> Result<Vec<String>, String> {
    let mut expr: Option<String> = None;
    let mut has_cycles = false;
    let mut out: Vec<String> = vec![
        "adapter=plotter".into(),
        "render=single".into(),
        "tui=off".into(),
    ];
    for arg in raw {
        if let Some(v) = arg.strip_prefix("cycles=") {
            has_cycles = true;
            out.push(format!("cycles={v}"));
        } else if let Some(v) = arg.strip_prefix("--mode=") {
            // The plotter adapter renders auto/plot/parametric/xy/polar.
            // Reject anything it can't draw rather than silently
            // mis-rendering it (`feedback_never_ignore_silently`).
            match v {
                "auto" | "plot" | "parametric" | "xy" | "polar" => {
                    out.push(format!("mode={v}"));
                }
                other => {
                    return Err(format!(
                        "unsupported --mode='{other}' \
                     (the plotter adapter renders: auto, plot, parametric, polar)"
                    ));
                }
            }
        } else if let Some(v) = arg.strip_prefix("--width=") {
            out.push(format!("width={v}"));
        } else if let Some(v) = arg.strip_prefix("--height=") {
            out.push(format!("height={v}"));
        } else if let Some(v) = arg.strip_prefix("output=") {
            // Each requested field gets its own lane (band).
            out.push(format!("lanes={}", v.replace(',', ";")));
        } else if arg == "--no-color" {
            out.push("no_color=true".into());
        } else if expr.is_none() && !arg.starts_with('-') {
            expr = Some(arg.clone());
        } else {
            return Err(format!(
                "unrecognized argument '{arg}'. wiring visualize accepts: \
                 <expr|file.polydat> cycles= output= --mode= --width= \
                 --height= --no-color"
            ));
        }
    }
    let expr = expr.ok_or("missing wiring expression or .polydat file argument")?;
    if !has_cycles {
        out.push("cycles=1000".into());
    }
    out.push(format!("op={}", read_expr_or_file(&expr)?));
    Ok(out)
}

/// A `.polydat` path is read as its source; anything else is the inline
/// expression itself (the `op=` parser accepts `;`-separated `:=`
/// bindings and auto-binds `cycle`).
fn read_expr_or_file(arg: &str) -> Result<String, String> {
    if arg.ends_with(".polydat") {
        std::fs::read_to_string(arg).map_err(|e| format!("failed to read '{arg}': {e}"))
    } else {
        Ok(arg.to_string())
    }
}

// ── cli_spec entry ─────────────────────────────────────────

/// `nmbrs wiring visualize <expr|file>` — sugar over
/// `nmbrs run adapter=plotter render=single`. The leaf parses raw
/// (`raw_args=true`), translates to run argv, and drives the engine.
const VISUALIZE_KV: &[crate::cli_spec::KvParam] = &[
    crate::completion::CYCLES_KV_PARAM,
    crate::completion::OUTPUT_KV_PARAM,
];

/// The flag surface `build_run_args` consumes.
fn visualize_flags() -> Vec<crate::cli_spec::Flag> {
    use crate::cli_spec::{Arity, Flag, ValueProvider};
    vec![
        Flag {
            long: "--mode",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "auto | plot | parametric | xy | polar.",
            repeatable: false,
        },
        Flag {
            long: "--width",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "Plot width in characters.",
            repeatable: false,
        },
        Flag {
            long: "--height",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "Plot height in characters.",
            repeatable: false,
        },
        Flag {
            long: "--no-color",
            short: None,
            aliases: &[],
            arity: Arity::Bool,
            value: ValueProvider::None,
            help: "Disable ANSI color output.",
            repeatable: false,
        },
    ]
}

pub fn spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle_visualize(p: ParsedCommand) -> Result<(), String> {
        let argv = build_run_args(&p.raw)?;
        let rt =
            tokio::runtime::Runtime::new().map_err(|e| format!("failed to start runtime: {e}"))?;
        rt.block_on(crate::run::run_command(&argv));
        Ok(())
    }
    fn handle_bare(_p: ParsedCommand) -> Result<(), String> {
        Err("expected `visualize` (try `nmbrs wiring visualize <expr|file>`)".into())
    }
    Command {
        name: "wiring",
        help: "Wiring toolset (`wiring visualize <expr|file>`).",
        category: Category::Tools,
        level: Level::FullSurface,
        flags: Vec::new(),
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        handler: Some(Handler::Sync(handle_bare)),
        raw_args: true,
        completion_override: None,
        subcommands: vec![Command {
            name: "visualize",
            help: "Plot a wiring expression in the terminal (sugar for run adapter=plotter).",
            category: Category::Tools,
            level: Level::FullSurface,
            flags: visualize_flags(),
            kv_params: VISUALIZE_KV,
            dynamic_options: None,
            positionals: Vec::new(),
            subcommands: Vec::new(),
            handler: Some(Handler::Sync(handle_visualize)),
            raw_args: true,
            completion_override: None,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Result<Vec<String>, String> {
        build_run_args(&raw.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn translates_expr_and_cycles_to_plotter_run() {
        let a = args(&["sin(cycle)", "cycles=200"]).unwrap();
        assert!(a.contains(&"adapter=plotter".to_string()));
        assert!(a.contains(&"render=single".to_string()));
        assert!(a.contains(&"cycles=200".to_string()));
        assert!(a.contains(&"op=sin(cycle)".to_string()));
    }

    #[test]
    fn defaults_cycles_when_omitted() {
        let a = args(&["sin(cycle)"]).unwrap();
        assert!(a.contains(&"cycles=1000".to_string()));
    }

    #[test]
    fn maps_visualize_flags_to_plotter_config() {
        let a = args(&[
            "x:=cos(cycle); y:=sin(cycle)",
            "--mode=parametric",
            "--width=80",
            "--no-color",
            "output=x,y",
        ])
        .unwrap();
        assert!(a.contains(&"mode=parametric".to_string()));
        assert!(a.contains(&"width=80".to_string()));
        assert!(a.contains(&"no_color=true".to_string()));
        assert!(a.contains(&"lanes=x;y".to_string()));
    }

    #[test]
    fn rejects_unsupported_mode() {
        let err = args(&["sin(cycle)", "--mode=histogram"]).unwrap_err();
        assert!(err.contains("histogram"), "{err}");
    }

    #[test]
    fn requires_an_expression() {
        assert!(args(&["cycles=10"]).is_err());
    }
}
