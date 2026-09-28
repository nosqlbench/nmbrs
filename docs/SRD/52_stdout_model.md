# 52: Stdout and Model Adapters

Lightweight adapters included in all binaries. Used for
workload development, debugging, and diagnostic output.

---

## Stdout Adapter

Renders resolved fields to a file or stdout.

### Configuration

```rust
pub struct StdoutConfig {
    pub filename: String,       // "stdout" or file path
    pub newline: bool,
    pub format: StdoutFormat,
    pub fields_filter: Vec<String>,  // empty = all fields
    pub separator: String,      // for `raw` format (custom `separator=`)
    pub header: bool,           // emit a header row (csv/tsv)
    pub color: bool,            // colorize output
}
```

### Formats

As built in `crates/nmbrs-adapter-stdout/src/lib.rs`, there are 7 formats:

| Format | Output |
|--------|--------|
| `stmt` | The `stmt` field (Statement). **DEFAULT for `nmbrs run`.** |
| `readout` | Aligned `name = value`, one per line |
| `assignments` | Compact `name=value` on one line |
| `json` | Typed values; `jsonl` is an alias |
| `csv` | Comma-separated values |
| `tsv` | Tab-separated values |
| `raw` | Values only, joined by the custom `separator=` |

`stmt` is the default. Select another via `format=json` on CLI or in
workload params.

### Field Rendering

The stdout adapter renders ALL fields in `ResolvedFields`, not
just a `stmt` field. This was a deliberate design decision:
adapter payloads may have multiple fields, and stdout should
show them all for diagnostic purposes.

The `fields_filter` parameter restricts output to named fields
when specified.

### Output paths and parent directories

When `filename` is anything other than `"stdout"`, the adapter
treats it as a filesystem path and:

- Creates parent directories on demand. `output=path/to/new/file.txt`
  works without a manual `mkdir -p` — equivalent to `mkdir -p
  path/to/new/` followed by `File::create("path/to/new/file.txt")`.
- Truncates an existing file at the path.
- Panics with the OS error if directory creation or file open
  still fails (e.g., permission denied, path collides with an
  existing non-directory). The diagnostic always includes both
  the path and the underlying `std::io::Error`.

Bare filenames in the cwd skip the directory step (they have no
parent component).

### Channel routing (per-op `stdout:`)

Per [SRD-40b §9](40b_synthetic_metrics_from_polydat.md), an op selects
where its rendered output is routed with a `stdout:` channel keyword:

- `stdout: terminal` — **default.** The rendered op is written to the
  console (or the configured file).
- `stdout: eventlog` — route through the event log (`diag!` Info),
  suppressing terminal/file output.
- `stdout: silent` — drop the output. The op still executes and any
  synthetic metrics still record.

### Output transport

Where the rendered op-output line actually GOES on the terminal is
owned by **[SRD-87](87_output_channel.md)** (the op-output bucket of
the single `OutputChannel`), not raw stdout. The stdout adapter is the
canonical op-output *producer*; routing its output through the channel
is what fixes the "stdout prints nothing on an interactive TTY" defect.

---

## Model Adapter

Simulation adapter for testing workload structure without a
live target. Renders operations like stdout but with additional
diagnostic capabilities.

### Configuration

```rust
pub struct ModelConfig {
    pub stdout: StdoutConfig,
    pub diagnose: bool,
}
```

When `diagnose=true`, the model adapter logs additional
information about field resolution, bind point substitution,
and template selection.

---

## Use Cases

- **Workload development**: `adapter=stdout format=json` to
  verify field resolution
- **Dry-run verification**: See what would be sent without a
  live target
- **Polydat testing**: Verify data generation patterns
- **CI validation**: Ensure workloads parse and resolve correctly
