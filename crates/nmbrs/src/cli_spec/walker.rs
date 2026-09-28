// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Generic argv walker that consumes a [`Command`] tree and
//! produces a [`ParsedCommand`] for the matched leaf.
//!
//! Algorithm:
//!
//! 1. Walk argv left-to-right, descending into subcommands as
//!    long as the next token matches a child name.
//! 2. At the deepest matched command, if `raw_args=true`,
//!    return immediately with the remainder in
//!    [`ParsedCommand::raw`].
//! 3. Otherwise, parse the remainder against the leaf's flags
//!    and positionals. Unknown flags are reported as errors;
//!    unknown positional shapes are accepted (handlers
//!    inspect [`ParsedCommand::positionals`] themselves).
//!
//! The walker accepts both `--flag value` and `--flag=value`
//! forms, plus aliases declared on each [`Flag`]. Bool flags
//! never consume the next token.

use super::*;

pub fn parse(root: &Command, argv: &[String]) -> Result<ParsedCommand, String> {
    // Help / version short-circuit: if `--help` / `-h` or
    // `--version` / `-V` appears anywhere in argv, walk the
    // subcommand path *up to* the earliest such flag, then
    // return immediately with the appropriate request bit set.
    // main.rs renders usage / prints the version string and
    // exits; handlers never see these flags.
    let help_at = argv.iter().position(|a| a == "--help" || a == "-h");
    let version_at = argv.iter().position(|a| a == "--version" || a == "-V");
    let short_at = match (help_at, version_at) {
        (Some(h), Some(v)) => Some(h.min(v)),
        (Some(h), None) => Some(h),
        (None, Some(v)) => Some(v),
        (None, None) => None,
    };
    let effective_end = short_at.unwrap_or(argv.len());

    let mut path: Vec<String> = vec![root.name.to_string()];
    let mut current: &Command = root;
    let mut i = 0usize;

    // Greedy subcommand descent within the pre-short-circuit
    // slice. Stop at the first token that doesn't name a
    // subcommand of the current node — that token becomes the
    // start of the leaf's argument tail.
    loop {
        if i >= effective_end {
            break;
        }
        let tok = &argv[i];
        let next = current.subcommands.iter().find(|s| s.name == tok);
        match next {
            Some(sub) => {
                path.push(sub.name.to_string());
                current = sub;
                i += 1;
            }
            None => break,
        }
    }

    if short_at.is_some() {
        return Ok(ParsedCommand {
            path,
            flags: BTreeMap::new(),
            bools: BTreeSet::new(),
            positionals: Vec::new(),
            raw: Vec::new(),
            argv: argv.to_vec(),
            help_requested: help_at.is_some(),
            version_requested: version_at.is_some(),
        });
    }

    let remaining = &argv[i..];

    // Raw-args escape hatch: handler gets the unparsed tail.
    if current.raw_args {
        return Ok(ParsedCommand {
            path,
            flags: BTreeMap::new(),
            bools: BTreeSet::new(),
            positionals: Vec::new(),
            raw: remaining.to_vec(),
            argv: argv.to_vec(),
            help_requested: false,
            version_requested: false,
        });
    }

    // Generic parse against the leaf's declared flags.
    let mut flags: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut bools: BTreeSet<String> = BTreeSet::new();
    let mut positionals: Vec<String> = Vec::new();
    let mut j = 0usize;

    while j < remaining.len() {
        let tok = &remaining[j];

        // Flag detection: `--name`, `--name=value`. Single-dash
        // short flags (`-c`) are matched against `Flag::short`
        // when declared.
        if let Some(rest) = tok.strip_prefix("--") {
            let (raw_name, inline_value) = match rest.split_once('=') {
                Some((n, v)) => (format!("--{n}"), Some(v.to_string())),
                None => (tok.clone(), None),
            };
            let f = lookup_flag(current, &raw_name)
                .ok_or_else(|| format!("unknown flag '{raw_name}' for `{}`", path.join(" ")))?;
            consume_flag(
                f,
                &raw_name,
                inline_value,
                remaining,
                &mut j,
                &mut flags,
                &mut bools,
            )?;
            continue;
        }
        if let Some(rest) = tok.strip_prefix('-')
            && !tok.starts_with("--")
            && !rest.is_empty()
        {
            // Single-dash short flag (e.g. `-c`).
            let raw_name = format!("-{rest}");
            let f = lookup_short(current, &raw_name);
            if let Some(f) = f {
                consume_flag(
                    f, &raw_name, None, remaining, &mut j, &mut flags, &mut bools,
                )?;
                continue;
            }
            // Fall through: treat as positional (some handlers
            // accept negative-number-shaped positionals).
        }

        positionals.push(tok.clone());
        j += 1;
    }

    // Validate required positionals. Optional / Many kinds
    // never error here; the handler decides when "missing" is
    // OK.
    let required = current
        .positionals
        .iter()
        .filter(|p| matches!(p.kind, PositionalKind::One))
        .count();
    if positionals.len() < required {
        let missing = current
            .positionals
            .iter()
            .skip(positionals.len())
            .filter(|p| matches!(p.kind, PositionalKind::One))
            .map(|p| format!("<{}>", p.name))
            .collect::<Vec<_>>()
            .join(" ");
        return Err(format!(
            "`{}` requires {} positional arg{} ({missing})",
            path.join(" "),
            required,
            if required == 1 { "" } else { "s" },
        ));
    }

    Ok(ParsedCommand {
        path,
        flags,
        bools,
        positionals,
        raw: Vec::new(),
        argv: argv.to_vec(),
        help_requested: false,
        version_requested: false,
    })
}

fn lookup_flag<'a>(cmd: &'a Command, name: &str) -> Option<&'a Flag> {
    cmd.flags
        .iter()
        .find(|f| f.long == name || f.aliases.contains(&name))
}

fn lookup_short<'a>(cmd: &'a Command, dashed: &str) -> Option<&'a Flag> {
    cmd.flags
        .iter()
        .find(|f| f.short.is_some_and(|s| s == dashed))
}

fn consume_flag(
    f: &Flag,
    raw_name: &str,
    inline_value: Option<String>,
    remaining: &[String],
    j: &mut usize,
    flags: &mut BTreeMap<String, Vec<String>>,
    bools: &mut BTreeSet<String>,
) -> Result<(), String> {
    match f.arity {
        Arity::Bool => {
            if inline_value.is_some() {
                return Err(format!(
                    "flag '{raw_name}' is boolean — does not accept a value"
                ));
            }
            bools.insert(f.long.to_string());
            *j += 1;
        }
        Arity::Value => {
            let value = match inline_value {
                Some(v) => {
                    *j += 1;
                    v
                }
                None => {
                    *j += 1;
                    if *j >= remaining.len() {
                        return Err(format!("flag '{raw_name}' requires a value"));
                    }
                    let v = remaining[*j].clone();
                    *j += 1;
                    v
                }
            };
            let entry = flags.entry(f.long.to_string()).or_default();
            if f.repeatable {
                entry.push(value);
            } else {
                // Non-repeatable: overwrite semantics — last
                // value wins. Same shape as most ad-hoc CLI
                // parsers. Use repeatable: true when you want
                // the full list.
                entry.clear();
                entry.push(value);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd_leaf(name: &'static str, flags: Vec<Flag>) -> Command {
        Command {
            name,
            help: "",
            category: Category::Tools,
            level: Level::Secondary,
            flags,
            kv_params: &[],
            dynamic_options: None,
            positionals: Vec::new(),
            subcommands: Vec::new(),
            handler: None,
            raw_args: false,
            completion_override: None,
        }
    }

    fn flag_value(long: &'static str) -> Flag {
        Flag {
            long,
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "",
            repeatable: false,
        }
    }

    fn flag_bool(long: &'static str) -> Flag {
        Flag {
            long,
            short: None,
            aliases: &[],
            arity: Arity::Bool,
            value: ValueProvider::None,
            help: "",
            repeatable: false,
        }
    }

    #[test]
    fn parses_long_flag_value_form() {
        let root = cmd_leaf("nmbrs", vec![flag_value("--db")]);
        let p = parse(&root, &["--db".into(), "x.db".into()]).unwrap();
        assert_eq!(p.flag("--db"), Some("x.db"));
    }

    #[test]
    fn parses_long_flag_eq_form() {
        let root = cmd_leaf("nmbrs", vec![flag_value("--db")]);
        let p = parse(&root, &["--db=x.db".into()]).unwrap();
        assert_eq!(p.flag("--db"), Some("x.db"));
    }

    #[test]
    fn collects_positionals() {
        let root = cmd_leaf("nmbrs", vec![flag_value("--db")]);
        let p = parse(
            &root,
            &[
                "--db".into(),
                "x.db".into(),
                "filter1".into(),
                "filter2".into(),
            ],
        )
        .unwrap();
        assert_eq!(p.positionals, vec!["filter1", "filter2"]);
    }

    #[test]
    fn bool_flags_dont_consume_value() {
        let root = cmd_leaf("nmbrs", vec![flag_bool("--tree"), flag_value("--db")]);
        let p = parse(&root, &["--tree".into(), "--db".into(), "x".into()]).unwrap();
        assert!(p.bool("--tree"));
        assert_eq!(p.flag("--db"), Some("x"));
    }

    #[test]
    fn rejects_unknown_flag() {
        let root = cmd_leaf("nmbrs", vec![]);
        let err = parse(&root, &["--bogus".into()]).unwrap_err();
        assert!(err.contains("unknown flag"));
    }

    #[test]
    fn version_short_circuit_sets_request_bit() {
        let root = cmd_leaf("nmbrs", vec![]);
        let p = parse(&root, &["--version".into()]).unwrap();
        assert!(p.version_requested);
        assert!(!p.help_requested);
        // Short alias -V works too.
        let p = parse(&root, &["-V".into()]).unwrap();
        assert!(p.version_requested);
    }

    #[test]
    fn version_short_circuits_before_unknown_flags() {
        // `--version` must short-circuit even when the rest of
        // argv would otherwise produce an unknown-flag error.
        let root = cmd_leaf("nmbrs", vec![]);
        let p = parse(&root, &["--version".into(), "--bogus".into()]).unwrap();
        assert!(p.version_requested);
    }

    #[test]
    fn descends_into_subcommand() {
        let list = cmd_leaf("list", vec![flag_value("--db")]);
        let metrics = Command {
            name: "metrics",
            help: "",
            category: Category::Tools,
            level: Level::Secondary,
            flags: Vec::new(),
            kv_params: &[],
            dynamic_options: None,
            positionals: Vec::new(),
            subcommands: vec![list],
            handler: None,
            raw_args: false,
            completion_override: None,
        };
        let root = Command {
            name: "nmbrs",
            help: "",
            category: Category::Tools,
            level: Level::Secondary,
            flags: Vec::new(),
            kv_params: &[],
            dynamic_options: None,
            positionals: Vec::new(),
            subcommands: vec![metrics],
            handler: None,
            raw_args: false,
            completion_override: None,
        };
        let p = parse(
            &root,
            &["metrics".into(), "list".into(), "--db".into(), "x".into()],
        )
        .unwrap();
        assert_eq!(p.path, vec!["nmbrs", "metrics", "list"]);
        assert_eq!(p.flag("--db"), Some("x"));
    }

    #[test]
    fn raw_args_passes_remainder_unparsed() {
        let mut leaf = cmd_leaf("run", vec![]);
        leaf.raw_args = true;
        let root = Command {
            name: "nmbrs",
            help: "",
            category: Category::Tools,
            level: Level::Secondary,
            flags: Vec::new(),
            kv_params: &[],
            dynamic_options: None,
            positionals: Vec::new(),
            subcommands: vec![leaf],
            handler: None,
            raw_args: false,
            completion_override: None,
        };
        let p = parse(
            &root,
            &["run".into(), "workload=x.yaml".into(), "cycles=100".into()],
        )
        .unwrap();
        assert_eq!(p.raw, vec!["workload=x.yaml", "cycles=100"]);
    }
}
