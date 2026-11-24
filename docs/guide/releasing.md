# Releasing nmbrs

A release versions every crate together, publishes them to crates.io, and
pushes a `v<version>` tag; the tag is what makes CI build the per-platform
binaries, the container image and the GitHub Release
(`.github/workflows/build.yml`). One command does it, and it publishes
nothing until the tree has passed every check on this page:

```sh
cargo ws-check              # the checks alone, against the tree as it stands
cargo ws-release            # clean main -> checks -> version, tag, publish
```

Both are cargo aliases (`.cargo/config.toml`) for the `xtask` crate at the
workspace root. `xtask/src/main.rs` holds the list of checks; this page
describes them.

## What you need installed

| Tool | For |
| --- | --- |
| `cargo-nextest` | the test checks |
| `cargo-deny` | the dependency policy check |
| `cargo-workspaces` | versioning and publishing (`cargo ws-release` only) |
| the C++ driver sysroot | every `--all-features` check; build it once with `bash crates/nmbrs-adapter-cql/build.sh driver` |

A check whose tool is missing fails by name; nothing is skipped.

## The release gate

`cargo ws-check` and `cargo ws-release` run the same checks, in this order,
and stop at the first that fails.

The workspace has two feature sets worth checking. The default set is what
crates.io builds: the pure-Rust CQL engine. `--all-features` is what the
released binaries and image are built from: both CQL engines. There is no
`--no-default-features` set, because `nmbrs-adapter-cql` refuses to compile
without an engine.

1. **rustfmt** — `cargo fmt --all --check`.
2. **clippy**, default features and all features —
   `cargo clippy --workspace --all-targets -- -D warnings`.
3. **rustdoc**, default features and all features —
   `cargo doc --workspace --no-deps` with `RUSTDOCFLAGS=-D warnings`, so a
   broken intra-doc link fails the release.
4. **dependency policy** — `cargo deny check`, under `deny.toml`: every
   vulnerability advisory is an error, a yanked crate is an error, every
   license must be on the allow-list, every crate must come from crates.io,
   and no requirement may be a wildcard. An advisory the project has
   accepted is listed in `deny.toml` with the reason and what would retire
   it. When this check fails on a new advisory, update the crate
   (`cargo update -p <crate>`); add it to the accepted list only when no
   fixed version can be reached.
5. **tests**, default features and all features —
   `cargo nextest run --workspace`, which includes the bundled example
   workloads, the dependency-rule gate (`architecture_rules`) and the PTY
   display suites. No test needs a database server.
6. **doc examples** — `cargo test --doc --workspace --all-features`, which
   nextest does not run.
7. **release build** —
   `cargo check --release --workspace --all-features --all-targets`.
8. **packaging** — `cargo package --workspace`: each publishable crate is
   packed as crates.io will receive it and built from that package alone,
   with its own default features. This is the only check that catches a
   file missing from a package (a bundled workload, a README), a feature
   that compiles only because another workspace member turned it on, and a
   requirement on a crate version that is not on crates.io.
   The check gives cargo its home directory with symlinks resolved:
   `nmbrs-web`'s template macro cannot find its templates when the
   unpacked crate is reached through a symlinked path.

CI runs `cargo ws-check` on every push and pull request (the `gate` job of
`.github/workflows/build.yml`), and the GitHub Release of a tag waits on it.

`cargo ws-check` runs the gate on whatever is on disk, so it is the way to
find out whether work in progress is releasable. `cargo ws-release` first
requires that the branch is `main` and that nothing is uncommitted, so that
what was checked is exactly what is tagged.

## Steps

1. **Publish the vendored driver fork first, if it changed.**
   `vendor/cassandra-cpp` is the crate `nmbrs-cassandra-cpp`. It is not a
   workspace member, so the release does not version or publish it. When
   its source changed, raise its version, raise the requirement on it in
   `crates/nmbrs-adapter-cql/Cargo.toml` and `crates/nmbrs/Cargo.toml`,
   and run `cargo publish` from `vendor/cassandra-cpp`. The packaging
   check fails if the version the workspace requires is not published.
2. **Release.** From a clean `main`, run `cargo ws-release`. Once the gate
   passes it asks for the version, offering the next patch, minor and major
   versions. `cargo ws-release minor`, `patch` or `custom X.Y.Z` names it
   instead; add `--dry-run` to rehearse it. After the gate it runs
   `cargo ws publish --force '*' --all --exact --allow-branch main` with
   the options given, which:
   - versions every crate together, at the version the workspace inherits
     (`[workspace.package] version`);
   - raises every internal requirement under `[workspace.dependencies]` to
     that version, exactly (`--exact`), so a host that updates one nmbrs
     crate gets the others it was built and tested with;
   - commits `Release <version>`, tags it `v<version>`, and pushes both;
   - publishes the crates in dependency order. `xtask` is versioned with
     them and never published.
3. **If a publish stops** on a crate whose dependency crates.io does not
   list yet (`failed to select a version for the requirement`), the index
   has not caught up with the crate published just before it. Wait a
   minute and publish the rest with `cargo publish -p <crate>`, in
   dependency order.
4. **Watch the tag build.** The pushed tag starts the `build` workflow,
   which attaches the binaries to the GitHub Release and pushes the image
   to GHCR. The crates are already published at this point; a failed
   platform build is fixed forward with a patch release.
