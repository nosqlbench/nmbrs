// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `nmbrs copy <name> [to=<path>]` — SRD-85 materialization.
//!
//! Copies a bundled workload's source to a local file for
//! editing. The copy is stamped with a provenance header
//! (catalog name + nmbrs version) so a diverged local copy can
//! always be traced to its origin — for humans and support
//! tooling, not a sync mechanism. Refuses to overwrite.
//!
//! The lighter-weight alternative is SRD-72 `extends:` — a
//! local child declaring `extends: <catalog-name>` inherits the
//! bundled parent without forking it.

/// Handle `nmbrs copy <name> [to=<path>]`.
pub fn copy_command(args: &[String]) -> Result<(), String> {
    // Closed surface: one bundled name plus optional `to=<path>`. Unknown
    // `--flags` and `key=` forms used to be silently ignored (a typo'd
    // `dest=x.yaml` wrote to the default path with no warning).
    for a in args {
        if a.starts_with('-') {
            return Err(format!(
                "unknown option '{a}'. usage: nmbrs copy <name> [to=<path>]"
            ));
        }
        if let Some((key, _)) = a.split_once('=')
            && key != "to"
        {
            return Err(format!(
                "unknown parameter '{key}='. usage: nmbrs copy <name> [to=<path>]"
            ));
        }
    }
    let name = args
        .iter()
        .find(|a| !a.starts_with('-') && !a.contains('='))
        .ok_or_else(|| {
            "usage: nmbrs copy <bundled-workload-name> [to=<path>]\n\
             `nmbrs describe workloads` lists what this binary carries."
                .to_string()
        })?;

    let bundled = nmbrs_workload::catalog::lookup(name).ok_or_else(|| {
        format!(
            "no bundled workload named `{name}` — `nmbrs describe workloads` \
             (or `--all` for the examples tier) lists the catalog.{}",
            // Copy runs bundled names only, so suggest from the catalog
            // alone — a local file would name something copy cannot act on.
            nmbrs_workload::suggest::did_you_mean(&nmbrs_workload::suggest::suggest_bundled(name))
        )
    })?;

    let dest: std::path::PathBuf = args
        .iter()
        .find_map(|a| a.strip_prefix("to="))
        .map(Into::into)
        .unwrap_or_else(|| {
            // Default: basename of the catalog name, flattened
            // (`cql/keyvalue` → `cql_keyvalue.yaml`) so the copy
            // lands in the cwd without surprise directories.
            std::path::PathBuf::from(format!("{}.yaml", bundled.name.replace('/', "_")))
        });

    if dest.exists() {
        return Err(format!(
            "refusing to overwrite existing file {} — pass `to=<path>` to \
             choose another destination",
            dest.display()
        ));
    }

    let provenance = format!(
        "# Copied from bundled workload `{}` (nmbrs {}).\n\
         # This copy is yours — edits here never sync back. To inherit the\n\
         # bundled parent instead of forking it, use `extends: {}`.\n",
        bundled.name,
        env!("CARGO_PKG_VERSION"),
        bundled.name,
    );
    // A shebang is only honored at byte 0: when the bundled source
    // opens with `#!/usr/bin/env nmbrs`, the interpreter line stays
    // FIRST and the provenance stamp goes directly under it, so the
    // materialized copy remains directly executable.
    let has_shebang = bundled.source.starts_with("#!");
    let content = if has_shebang {
        match bundled.source.split_once('\n') {
            Some((bang, rest)) => format!("{bang}\n{provenance}{rest}"),
            None => format!("{}\n{provenance}", bundled.source),
        }
    } else {
        format!("{provenance}{}", bundled.source)
    };
    std::fs::write(&dest, content).map_err(|e| format!("write {}: {e}", dest.display()))?;
    // Shebang'd workloads ship executable: add exec bits for every
    // class that already has read, mirroring the source's intent.
    #[cfg(unix)]
    if has_shebang {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&dest).map_err(|e| format!("stat {}: {e}", dest.display()))?;
        let mut perms = meta.permissions();
        let mode = perms.mode();
        perms.set_mode(mode | ((mode & 0o444) >> 2));
        std::fs::set_permissions(&dest, perms)
            .map_err(|e| format!("chmod {}: {e}", dest.display()))?;
    }
    println!(
        "copied bundled workload `{}` to {}",
        bundled.name,
        dest.display()
    );
    Ok(())
}

/// `to=<path>` is free-form (a new filename); listed so the
/// option completes.
static COPY_KV_PARAMS: &[crate::cli_spec::KvParam] = &[crate::cli_spec::KvParam {
    key: "to=",
    provider: crate::completion::free_form,
}];

pub fn spec() -> crate::cli_spec::Command {
    use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
    fn handle(p: ParsedCommand) -> Result<(), String> {
        copy_command(&p.raw)
    }
    Command {
        name: "copy",
        help: "Copy a bundled workload to a local file for editing (provenance-stamped).",
        category: Category::Documentation,
        level: Level::FullSurface,
        flags: Vec::new(),
        kv_params: COPY_KV_PARAMS,
        dynamic_options: None,
        positionals: vec![crate::cli_spec::Positional {
            name: "name",
            help: "Bundled workload to copy (catalog name).",
            kind: crate::cli_spec::PositionalKind::One,
            value: crate::cli_spec::ValueProvider::Custom(crate::completion::catalog_name_provider),
        }],
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: true,
        completion_override: None,
    }
}
