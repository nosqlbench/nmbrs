// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Repository tasks, run through the cargo aliases in
//! `.cargo/config.toml`.
//!
//! `cargo ws-check` runs `xtask gate`: every check a release must pass,
//! against the tree as it stands, committed or not. Nothing is versioned
//! or published. `cargo ws-check --quick` runs the same checks but packs
//! the crates without building them from their packages, which is half
//! the gate's time; it is what CI runs on everything but a release tag.
//!
//! `cargo ws-release [bump] [cargo-workspaces options]` runs
//! `xtask release`: it requires a clean `main`, runs the same gate, and
//! then, only when every check passes, runs `cargo ws publish` with the
//! release's fixed options and the ones given
//! (`docs/guide/releasing.md`). With no bump, `cargo ws publish` asks for
//! the version, offering the next patch, minor and major versions.
//! `--dry-run` passes through to `cargo ws publish`, so a rehearsal is
//! gated the same way.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// One check of the release gate: what it is called and the cargo
/// arguments that run it.
struct Check {
    name: &'static str,
    args: &'static [&'static str],
    /// Set `RUSTDOCFLAGS=-D warnings`, so a rustdoc warning fails the
    /// check.
    rustdoc_warnings_are_errors: bool,
    /// The check refuses an uncommitted tree unless told otherwise;
    /// `cargo ws-check` adds `--allow-dirty` to it, `cargo ws-release`
    /// does not.
    refuses_dirty_tree: bool,
    /// The check packs each crate and then builds it from its package. The
    /// build compiles workspace crates out of cargo's registry directory,
    /// so it runs with [`physical_cargo_home`] and, because of that, in
    /// [`PACKAGING_TARGET_DIR`]; [`Depth::Quick`] packs without building
    /// (`--no-verify`).
    builds_packed_crates: bool,
}

const fn check(name: &'static str, args: &'static [&'static str]) -> Check {
    Check {
        name,
        args,
        rustdoc_warnings_are_errors: false,
        refuses_dirty_tree: false,
        builds_packed_crates: false,
    }
}

const fn rustdoc(name: &'static str, args: &'static [&'static str]) -> Check {
    Check {
        name,
        args,
        rustdoc_warnings_are_errors: true,
        refuses_dirty_tree: false,
        builds_packed_crates: false,
    }
}

const fn packaging(name: &'static str, args: &'static [&'static str]) -> Check {
    Check {
        name,
        args,
        rustdoc_warnings_are_errors: false,
        refuses_dirty_tree: true,
        builds_packed_crates: true,
    }
}

/// The release gate, in the order a failure is cheapest to find.
///
/// The workspace has two feature sets worth checking: the default one,
/// which is what crates.io builds (the pure-Rust CQL engine), and
/// `--all-features`, which is what the released binaries and image are
/// built from (both CQL engines; it needs the C++ driver sysroot from
/// `bash crates/nmbrs-adapter-cql/build.sh driver`). There is no
/// `--no-default-features` set: `nmbrs-adapter-cql` refuses to compile
/// without an engine.
const GATE: &[Check] = &[
    check("rustfmt", &["fmt", "--all", "--check"]),
    check(
        "clippy (default features)",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    ),
    check(
        "clippy (all features)",
        &[
            "clippy",
            "--workspace",
            "--all-features",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    ),
    rustdoc(
        "rustdoc (default features)",
        &["doc", "--workspace", "--no-deps"],
    ),
    rustdoc(
        "rustdoc (all features)",
        &["doc", "--workspace", "--no-deps", "--all-features"],
    ),
    // Advisories, licenses, sources and wildcard requirements, under the
    // policy in deny.toml.
    check("dependency policy", &["deny", "check"]),
    check(
        "tests (default features)",
        &["nextest", "run", "--workspace"],
    ),
    check(
        "tests (all features)",
        &["nextest", "run", "--workspace", "--all-features"],
    ),
    // The rustdoc examples, which nextest does not run.
    check(
        "doc examples",
        &["test", "--doc", "--workspace", "--all-features"],
    ),
    check(
        "release build",
        &[
            "check",
            "--release",
            "--workspace",
            "--all-features",
            "--all-targets",
        ],
    ),
    // Each publishable crate, packed as crates.io will receive it and
    // built from that package alone with its own default features: the
    // one check that catches a file missing from a package, a feature
    // that only compiles because another member turned it on, and a
    // requirement on a crate version that is not published (the vendored
    // `nmbrs-cassandra-cpp` fork). The build shares nothing with the checks
    // above, so it is the slowest check by far; `Depth::Quick` keeps the
    // packing (manifests, file lists, requirement resolution) and leaves
    // the build to the full gate.
    packaging(
        "packaging",
        &["package", "--workspace", "--exclude", "xtask"],
    ),
];

/// The options every release passes to `cargo ws publish`: version
/// every crate together (`--force '*'`, `--all`), pin the internal
/// requirements to the released version (`--exact`), and release only
/// from `main`.
const PUBLISH: &[&str] = &[
    "ws",
    "publish",
    "--force",
    "*",
    "--all",
    "--exact",
    "--allow-branch",
    RELEASE_BRANCH,
];

/// The only branch a release is made from.
const RELEASE_BRANCH: &str = "main";

const USAGE: &str = "usage:
  cargo ws-check [--quick]
      run the release gate against the tree as it stands; --quick packs
      the crates without building them from their packages
  cargo ws-release [patch|minor|major|custom X.Y.Z] [cargo-workspaces options]
      require a clean main, run the release gate, then publish";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let passed = match args.split_first() {
        Some((task, [])) if task == "gate" => {
            gate(&workspace_root(), Tree::AsItStands, Depth::Full)
        }
        Some((task, [flag])) if task == "gate" && flag == "--quick" => {
            gate(&workspace_root(), Tree::AsItStands, Depth::Quick)
        }
        Some((task, rest)) if task == "release" => release(&workspace_root(), rest),
        _ => {
            eprintln!("{USAGE}");
            false
        }
    };
    if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// What the gate may assume about the working tree.
#[derive(Clone, Copy, PartialEq)]
enum Tree {
    /// `cargo ws-check`: whatever is on disk, committed or not.
    AsItStands,
    /// `cargo ws-release`: already verified clean.
    Clean,
}

/// How much of the gate runs.
#[derive(Clone, Copy, PartialEq)]
enum Depth {
    /// Everything; the only depth a release is cut at.
    Full,
    /// `cargo ws-check --quick`: every check, but the crates are packed
    /// without being built from their packages.
    Quick,
}

/// Run every check of [`GATE`] in order and stop at the first that
/// fails. Returns whether all of them passed.
fn gate(root: &Path, tree: Tree, depth: Depth) -> bool {
    for (index, check) in GATE.iter().enumerate() {
        eprintln!(
            "\n==> release gate {}/{}: {}",
            index + 1,
            GATE.len(),
            check.name
        );
        let mut command = cargo(root);
        command.args(check.args);
        if check.refuses_dirty_tree && tree == Tree::AsItStands {
            command.arg("--allow-dirty");
        }
        if check.builds_packed_crates {
            match depth {
                Depth::Quick => {
                    command.arg("--no-verify");
                }
                Depth::Full => {
                    command
                        .arg("--target-dir")
                        .arg(root.join(PACKAGING_TARGET_DIR));
                    if let Some(home) = physical_cargo_home() {
                        command.env("CARGO_HOME", home);
                    }
                }
            }
        }
        if check.rustdoc_warnings_are_errors {
            command.env("RUSTDOCFLAGS", "-D warnings");
        }
        if !succeeds(&mut command) {
            eprintln!("\nthe release gate failed at: {}", check.name);
            return false;
        }
    }
    match depth {
        Depth::Full => eprintln!("\n==> release gate passed ({} checks)", GATE.len()),
        Depth::Quick => eprintln!(
            "\n==> release gate passed ({} checks; quick: the packed crates were not built)",
            GATE.len()
        ),
    }
    true
}

/// Require a clean `main`, run the gate, then publish with
/// `publish_args` after the fixed options. With no `publish_args`,
/// `cargo ws publish` prompts for the version on the terminal this task
/// inherits.
fn release(root: &Path, publish_args: &[String]) -> bool {
    // `cargo ws publish` refuses a dirty tree and any branch but main
    // itself; asking first means the refusal comes before the gate has
    // run rather than after it.
    if let Err(reason) = releasable_tree(root) {
        eprintln!("cargo ws-release: {reason}; nothing was checked, versioned or published");
        return false;
    }
    if !gate(root, Tree::Clean, Depth::Full) {
        eprintln!("cargo ws-release: nothing was versioned or published");
        return false;
    }
    eprintln!("\n==> running cargo ws publish {}", publish_args.join(" "));
    let mut command = cargo(root);
    command.args(PUBLISH).args(publish_args);
    succeeds(&mut command)
}

/// Whether the working tree is one a release can be cut from: on
/// [`RELEASE_BRANCH`] with nothing uncommitted. The error says which of
/// the two it is not.
fn releasable_tree(root: &Path) -> Result<(), String> {
    let branch = git(root, &["branch", "--show-current"])?;
    if branch != RELEASE_BRANCH {
        return Err(format!(
            "a release is cut from {RELEASE_BRANCH}, and this is {}",
            if branch.is_empty() {
                "a detached HEAD"
            } else {
                branch.as_str()
            }
        ));
    }
    let uncommitted = git(root, &["status", "--porcelain"])?;
    if !uncommitted.is_empty() {
        return Err(format!(
            "the working tree has uncommitted changes:\n{uncommitted}\ncommit them first \
             (`cargo ws-check` runs the gate without this requirement)"
        ));
    }
    Ok(())
}

/// The trimmed standard output of a read-only `git` query in `root`.
fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|e| format!("could not run git {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Where the packaging check builds, relative to the workspace root: a
/// build directory of its own.
///
/// Cargo keys a dependency's build artifacts by a hash that does not
/// include the path its source was read from, while the artifact's
/// contents do depend on it. The packaging check may read the registry
/// through a different path than every other cargo command
/// ([`physical_cargo_home`]), so in the shared `target/` it would overwrite
/// dependency artifacts in place with differently-hashed builds and leave
/// the rest of the cache referring to crates that are no longer there
/// (`can't find crate for …` on the next ordinary build).
const PACKAGING_TARGET_DIR: &str = "target/packaging-gate";

/// Cargo's home directory with every symlink resolved, or `None` when it
/// cannot be found, in which case cargo is left to find its own.
///
/// The packaging check unpacks each workspace crate under
/// `$CARGO_HOME/registry/src` and compiles it there. `nmbrs-web`'s
/// template derive (askama) locates its templates by a path relative to
/// the source file, computed from the resolved location of one and the
/// unresolved location of the other, so when the home directory is
/// reached through a symlink the path points nowhere and the crate does
/// not compile. Handing cargo the resolved directory makes the two agree.
fn physical_cargo_home() -> Option<PathBuf> {
    let home = match std::env::var_os("CARGO_HOME") {
        Some(home) => PathBuf::from(home),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".cargo"),
    };
    home.canonicalize().ok()
}

/// The cargo that runs this task, so the gate builds with the same
/// toolchain, in the workspace root.
fn cargo(root: &Path) -> Command {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command.current_dir(root);
    command
}

/// Whether `command` ran and exited successfully; a command that
/// cannot start, such as a missing `cargo nextest`, fails by name.
fn succeeds(command: &mut Command) -> bool {
    match command.status() {
        Ok(status) => status.success(),
        Err(e) => {
            eprintln!("could not run {command:?}: {e}");
            false
        }
    }
}

/// The workspace root: the directory above this crate's.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace root")
        .to_path_buf()
}
