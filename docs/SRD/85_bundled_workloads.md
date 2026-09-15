# SRD 85: Bundled Workloads — Catalog, Discovery, and Materialization

**Status: SHIPPED (P1+P2+P3, 2026-06-11).** Catalog types +
registry in `nmbrs-workload/src/catalog.rs`; embedding generated
by `nmbrs/build.rs` alone — the binary is the only catalog
assembler, so it is the only generator (adapter directories are
included there behind the same feature gates that compile the
adapters in, read as `CARGO_FEATURE_*`); resolution + ambiguity
error in
`nmbrs-runtime::runner::resolve_workload`; bundled loading
through `extends::load_and_merge_bundled` with catalog-name
session identity; `nmbrs describe workloads`
(list/detail/`--all`/`examples`/`--json`) in `nmbrs/src/describe.rs`;
`nmbrs copy` in `nmbrs/src/copy_cmd.rs`; shell completion falls
back to catalog sources for param/scenario suggestions. P3
landed with it: `extends:` targets resolve local-first then
catalog, with **namespace-relative sibling resolution** for
bundled origins (a bundled `cql/x` extending `y.yaml` finds
`cql/y` — the sibling-by-filename idiom works identically on
disk and in the catalog). The curated tier ships with
`selfcheck` and `capacity_probe` plus the `cql/` suite
(`baselinesv3/{keyvalue,tabular,timeseries}` — native nmbrs
ports of the heritage baselines, op shapes only — plus the
vector suite and compaction test), all carrying
`description:`. Coverage:
`nmbrs/tests/bundled_workloads.rs` (bare-directory runs, tier
listing, lint, copy, ambiguity, extends-from-bundled).
Implementation deltas from the draft are folded into the
sections below (the curated lint runs as a CI-gating e2e test
over `describe workloads --json` rather than a build.rs step;
`copy` flattens namespaced default filenames).

## Motivation

nmbrs distributes as single-binary artifacts (multi-platform
binaries, docker images). A user who receives an artifact today
receives **zero workloads**: workload resolution is
cwd-relative only, and discovery means browsing the git repo.
Two distinct audiences are underserved:

1. **Operators** looking for curated, real-world workloads they
   can run and adapt — the "what can this thing do for me"
   entry point.
2. **Artifact validation** — many of the repo's example
   workloads are useful for smoke-testing a built artifact
   *as-is*, on any machine, with no checkout. Today that
   requires the repo.

These audiences must not be conflated: examples and coverage
workloads are teaching documents and spec pins, and exposing
them in the same listing as curated workloads buries the real
products in fifty test fixtures. The catalog therefore has
**visibility tiers**: everything bundled is runnable by name,
but only the curated tier is *listed* by default.

The nb5 heritage surface (`--list-workloads`,
`--list-scenarios`, `--copy <name>`, classpath-bundled yamls)
is the proven shape; this SRD restates it in nmbrs idiom.

One layer down, the precedent already exists in-tree: polydat
stdlib modules are `include_str!`-embedded with name-based
resolution (`polydat/src/dsl/compile.rs` STDLIB table). Bundled
workloads are the same mechanism one level up.

## Surface

### Naming and namespaces

Every bundled workload has a **catalog name**:
`<namespace>/<name>`, no extension. Namespaces are owned by the
source location:

| Namespace | Source | Tier |
|-----------|--------|------|
| *(top level, no slash)* and per-domain groups | `nmbrs/workloads/` (inside the binary's package, so a crates.io build bundles it) | **curated** |
| `<adapter>/…` (e.g. `cql/…`) | `nmbrs/workloads/<adapter>/` | **curated**, present only when the adapter is compiled in |
| `examples/…` | `nmbrs/examples/workloads/` | **examples** — bundled, runnable, unlisted by default |

`workload=cql/keyvalue` and
`workload=examples/cursors/timeboxed_partition_sweep` run a bundled
workload by name. Bare curated names (`workload=keyvalue_smoke`)
resolve at the top level of the catalog.

### Resolution order — nearest-first, never silent shadowing

Every reference-linking mechanism (`workload=`, `extends:`,
`implements:`, `impl=`, `driver=`) resolves in the same
nearest-first order, and the **logical filesystem location is
favored by default**:

1. A file beside the referring document (secondary refs only —
   the referring file's own directory).
2. The cwd's logical layout (exact path, extension probing, the
   cwd `workloads/` subdirectory; `drivers/<n>/driver.yaml` for
   drivers).
3. The bundled catalog: exact name, then the referring bundled
   document's namespace (the sibling-by-filename idiom), then
   the bare stem.

A name that resolves in **more than one** place is a *warnable
condition*, not an error: the nearest candidate wins and a
warning names every match — shadowing is allowed but never
silent. This keeps a fresh checkout authoritative over a stale
binary's embedded catalog (the failure mode that motivated the
rule), while the warning prompts the operator toward unique
names. `--strict` promotes resolution warnings to hard errors
(restoring the old fail-on-ambiguity behavior). An explicitly
pinned reference — a `./`/`../` path that resolves at its
pinned location, or a full catalog name with no local match —
is unambiguous and warns about nothing.

### Discovery — `nmbrs describe workloads`

Discovery rides the existing `describe` topic dispatch
(`nmbrs/src/describe.rs`), not a new subcommand family:

```
nmbrs describe workloads                  # curated tier: name, description, adapter(s)
nmbrs describe workloads --all            # + examples tier
nmbrs describe workloads examples         # examples tier only
nmbrs describe workloads cql/keyvalue     # one workload in detail
```

The detail view renders the workload's `description:`, its
scenarios (names + per-scenario structure), its params with
defaults (SRD-60 already makes params discoverable), required
adapter(s), and the run line to start from. The single-name
form accepts local paths too — `nmbrs describe workloads
./mine.yaml` introspects an un-bundled file with the same
renderer.

### Self-description — the `description:` field

The workload model gains an optional top-level `description:`
string (first line = listing summary; rest = detail-view body).
**Curated-tier workloads are required to carry one** — enforced
by a CI-gating test over `describe workloads --json` (the
`described` flag distinguishes the structured field from the
comment fallback), so a curated workload without a description
fails the test suite, not the user.
Examples may rely on their header comments; the detail view
falls back to the first comment block when `description:` is
absent.

### Materialization — `nmbrs copy <name>`

```
nmbrs copy cql/baselinesv3/keyvalue       # writes ./cql_baselinesv3_keyvalue.yaml
nmbrs copy cql/baselinesv3/keyvalue to=my_test.yaml
```

(The default filename flattens the catalog name's slashes to
underscores so the copy lands in the cwd without surprise
directories.)

Copies the bundled source to a local file for editing. Refuses
to overwrite an existing file. The copy is stamped with a
provenance header comment (catalog name + nmbrs version) so a
diverged local copy can always be traced to its origin.

The lighter-weight alternative to copying is SRD-72
`extends:` — see §Interactions.

### Examples tier and artifact validation

The examples tier exists so a bare artifact can validate
itself anywhere: every bundled example is standalone-runnable
(the repo already enforces this — see the "examples run
standalone" convention), so

```
nmbrs run workload=examples/signals/lfsr
```

works on a machine with nothing but the binary. CI and proof
harnesses iterate the catalog programmatically
(`describe workloads --all --json`, P2) instead of carrying
path lists. Coverage-pair workloads
(`examples/<theme>_coverage.yaml`) ship in this tier too —
their spec-pin role lives in the paired test files, but the
yamls themselves are legitimate smoke material.

What stays repo-only: anything not under the three source
directories (scratch workloads, `local/` dirs, design-memo
fixtures).

## Internal model

### Embedding

The binary's own `build.rs` is the single generator — there is
exactly one catalog assembler (the artifact), so per-crate
manifests would be structure without a consumer. It walks the
source directories and writes one static table into `OUT_DIR`,
mirroring the stdlib precedent:

```rust
pub struct BundledWorkload {
    pub name: &'static str,        // catalog name, e.g. "cql/keyvalue"
    pub tier: Tier,                // Curated | Example
    pub source: &'static str,      // include_str! of the yaml
}
```

- `nmbrs/workloads/` (curated, top-level names) and
  `nmbrs/examples/workloads/` (examples tier) are always embedded.
- `nmbrs/workloads/<a>/` subtrees are embedded under the
  adapter's namespace **only when the adapter's feature is
  enabled** — build scripts see their crate's features as
  `CARGO_FEATURE_*` env vars, so a build without the CQL engine
  has no `cql/` namespace and `describe workloads` is truthful
  about what *this* binary can run. (Adapters do not carry
  their own generators: what ships in the artifact is the
  binary's decision, and all adapters are in-workspace.)
- The catalog is installed once at startup; lookup is
  exact-name. No globbing, no fuzzy match — `describe
  workloads` is the discovery surface, not the resolver.
- Walk rules: only `.yaml`/`.yml`; `_`-prefixed files skipped;
  subdirectories recurse into name segments except `local/`
  and `logs/`; duplicate catalog names are a build error;
  `cargo:rerun-if-changed` covers every visited path.

### Loading

The workload loader gains a source abstraction no wider than
"named string": resolution returns either a path (read file, as
today) or a `(catalog_name, &'static str)` pair (parse from
memory). Everything downstream — parse, validation, sessions,
checkpoint identity — sees the same parsed model. Session
metadata records the catalog name + artifact version as the
workload identity for bundled runs (instead of a filesystem
path), so `session.log` and phase outcomes stay traceable.

`extends:` references resolve through the same two-step order
(local first, then catalog; both at once is the ambiguity
error). Catalog candidates are tried as the exact name
(extension-stripped — files extend siblings by filename,
catalog names carry none), then namespace-relative for bundled
origins, letting bundled families reference each other by plain
sibling filename.

## Interactions

- **SRD-60 (CLI)** — owns the `workload=` parameter and the
  params-discoverability contract the detail view renders.
  `describe workloads` extends the SRD-32a describe surface.
- **SRD-72 (`extends:`)** — the multiplier. A user extends a
  bundled parent (`extends: cql/keyvalue`) instead of copying
  and diverging; the catalog gives `extends:` a stable,
  versioned parent universe. The two designs should land
  adjacent; P3 here depends on SRD-72's implementation.
- **Build pipeline** — bundles ride the existing feature-gated
  build matrix; no new artifact types. Binary-size cost is
  yaml text and is bounded by curation.

## Non-goals

- **Remote registries / fetching.** The catalog is what's in
  the artifact. A network story is a different design with
  different trust questions.
- **Auto-updating copied workloads.** A copy is a fork; the
  provenance header is for humans and support tooling, not a
  sync mechanism.
- **In-place editing of bundled workloads.** Bundled sources
  are immutable; `copy` (or `extends:`) is the modification
  path.
- **Fuzzy name matching.** Exact catalog names only; discovery
  handles the "what's it called" problem.

## Phased delivery

**P1 — Catalog + resolution + discovery. (SHIPPED)** Embedding manifests
(core + adapters), `resolve_workload_file` catalog step with
the ambiguity error, `describe workloads` list/detail/`--all`,
the `description:` model field + curated-tier lint. A first
curated set under `workloads/` (even just 2-3 real-world
workloads) to make the tier non-hypothetical.

**P2 — Materialization + machine surface. (SHIPPED)** `nmbrs copy` with
provenance stamping; `describe workloads --json` for CI /
proof-harness iteration; session metadata carries catalog
identity for bundled runs.

**P3 — Composition. (SHIPPED)** SRD-72 was already implemented
(audited 2026-06-10), so `extends:`-through-catalog landed with
P1: local children extend bundled parents by catalog name, and
bundled siblings extend each other by filename via
namespace-relative resolution.

## See also

- [SRD 60](60_cli.md) — CLI parameter surface, params
  discoverability.
- [SRD 72](72_workload_extends.md) — single-parent workload
  composition.
- [SRD 20](20_workload_model.md) — the workload model the
  `description:` field joins.
- nb5 heritage: `--list-workloads`, `--list-scenarios`,
  `--copy` in `nb-engine-cli/.../NBCLIOptions.java`.
