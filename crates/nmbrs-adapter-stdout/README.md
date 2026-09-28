# nmbrs-adapter-stdout

The `stdout` adapter for [nmbrs](https://crates.io/crates/nmbrs). It renders each
op's resolved fields as text and writes them to the console or to a file. It
sends nothing to a remote system, so it is the usual way to check what a
workload generates before pointing it at a real target.

## Using it

This adapter is used through the `nmbrs` CLI. Select it with `adapter=stdout`.
It is also the default: a run that names no adapter uses `stdout`.

```bash
nmbrs run workload=crates/nmbrs/examples/workloads/getting_started/inline_ops.yaml cycles=8
nmbrs run workload=my_workload.yaml adapter=stdout format=json filename=out/ops.jsonl
```

### Adapter parameters

Set these on the command line (`key=value`) or in the workload's `params:`.

| Param | Default | Effect |
|-------|---------|--------|
| `filename` | `stdout` | Output file. Parent directories are created as needed. Any value other than `stdout` writes to that file. |
| `format` | `stmt` | Output format (see below). An unknown name is rejected when the adapter is created. |
| `separator` | `,` | Field separator for the `raw` format. |
| `header` | `false` | `true`, `1` or `yes` writes a row of field names before the first line (`csv`, `tsv`, `raw` only). |
| `color` | `false` | `true`, `1` or `yes` replaces `{red}`, `{green}`, `{yellow}`, `{blue}`, `{magenta}`, `{cyan}`, `{white}`, `{bold}`, `{dim}`, `{underline}` and `{reset}` in the rendered text with ANSI escape codes. |

Formats:

| `format=` | Output per op |
|-----------|---------------|
| `stmt` (alias `statement`) | The `stmt` field. If there is none, the `raw` field, then `prepared`, then every field value on its own line. |
| `readout` | `name = value`, one field per line, aligned on `=`. |
| `assignments` (alias `assign`) | `name=value, name=value` on one line. |
| `json` (aliases `jsonl`, `inlinejson`) | One JSON object per op. Numbers and booleans keep their types. |
| `csv` | Values separated by commas. A value that contains a comma, quote or newline is quoted. |
| `tsv` | Values separated by tabs. Tabs inside values become `\t`. |
| `raw` | Values joined with `separator`. |

Fields are written in the iteration order of the op template's field map, which
is not necessarily the order they were declared in.

While `stdout` writes to the console it asks nmbrs to turn the dashboard TUI
off. With `filename=` set, the console is free and the TUI can stay on.

### Op fields

The adapter reads no specific op fields. It renders every field of the op
template, after substituting `{name}` bind points with that cycle's values. An
op written for another adapter (with `stmt`, `method`, `uri` and so on) can be
dry-run through `stdout` as-is.

From [`getting_started/inline_ops.yaml`](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs/examples/workloads/getting_started/inline_ops.yaml):

```yaml
params:
  adapter: stdout

bindings: |
  input cycle: u64
  user_id := mod(hash(cycle), 1000000)

ops:
  # 3 reads per 1 write
  read:
    ratio: 3
    stmt: "read id={user_id}"
  write:
    ratio: 1
    stmt: "write id={user_id}"
```

### Op parameter: `stdout`

One op-template parameter, `stdout`, chooses where that op's rendered text
goes:

| Value | Aliases | Effect |
|-------|---------|--------|
| `terminal` | `stdout`, `default` | Write to the console or `filename` (the default). |
| `eventlog` | `log`, `diag` | Send the line to the nmbrs event log at Info level instead. Nothing is written to the console or file. |
| `silent` | `drop`, `discard`, `none` | Write nothing. The op still runs, and its metrics are still recorded. |

Any other value, or a non-string value, is an error when the op is mapped.
From [`metrics/synthetic_metrics.yaml`](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs/examples/workloads/metrics/synthetic_metrics.yaml),
where the op exists to publish a metric and the rendered line is incidental:

```yaml
    ops:
      synth_op_simple:
        adapter: stdout
        params:
          stdout: eventlog
        stmt: "simple[load={load}]"
        metrics: load
```

### Results

Each op produces a text result body that holds the rendered line, whichever
channel was chosen.

## Cargo features

None.

## Where it sits

- Implements `DriverAdapter` and `OpDispenser` from
  [nmbrs-runtime](https://crates.io/crates/nmbrs-runtime) and registers itself
  under the name `stdout` via `inventory`, so linking the crate into a binary is
  enough to make `adapter=stdout` available.
- Reads op templates (`ParsedOp`) from
  [nmbrs-workload](https://crates.io/crates/nmbrs-workload).
- For Rust callers, the crate exports `StdoutAdapter`, `StdoutConfig`,
  `StdoutFormat`, `StdoutChannel` and `FORMAT_NAMES`.
  [nmbrs-adapter-testkit](https://crates.io/crates/nmbrs-adapter-testkit) uses
  `StdoutConfig` and `StdoutFormat` for its own output.

## Links

- Repository: https://github.com/nosqlbench/nmbrs
- API docs: https://docs.rs/nmbrs-adapter-stdout
- nmbrs CLI: https://crates.io/crates/nmbrs

## License

Apache-2.0
