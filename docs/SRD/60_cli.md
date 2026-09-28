# 60: CLI Structure

The nmbrs CLI provides workload execution, Polydat benchmarking,
diagnostic tools, and shell completions.

---

## Command Tree

```
nmbrs
├── run           Execute a workload
│   adapter=<name> workload=<file.yaml> cycles=N concurrency=N
│   tags=<filter> rate=N format=<type>
│   op="<inline statement>"
│
├── bench         Polydat kernel micro-benchmark
│   <expr>        Polydat expression to benchmark
│   cycles=N concurrency=N --explain
│   <file.polydat>    Benchmark a Polydat module file
│
├── web           Start/stop web UI
│   --daemon --stop --restart
│   bind=<addr> port=<port>
│
├── describe      Inspect workload/GK structure
│   workload <file.yaml>
│   Polydat stdlib
│
├── plot          Evaluate and render Polydat outputs to terminal
│   Polydat <expr|file.polydat> [cycles=N] [--width=N] [--height=N]
│                 [--mode=plot|histogram] [--no-color]
│                 [--xscale=N] [--yscale=N] [--max-labels=N]
│
└── <file.yaml>   Bare file invocation → auto-detect run
```

### Bare File Invocation

```
nmbrs myworkload.yaml tags=phase:rampup cycles=1000
```

Equivalent to `nmbrs run workload=myworkload.yaml ...`. The CLI
detects `.yaml`/`.yml` extensions and routes to `run`.

---

## Parameter Passing

All parameters use `key=value` syntax:

```
nmbrs run adapter=cql hosts=127.0.0.1 workload=cql_vector.yaml \
  tags=phase:search cycles=100 concurrency=100
```

No `--key value` form for workload/adapter params. Flags like
`--explain` and `--daemon` use standard flag syntax.

### Known Parameters

The runner validates all parameters at startup. Unrecognized
parameters produce a hard error:

```
error: unrecognized parameter(s): 'trhreads'. Check for typos.
```

Known parameter categories:
- **Activity**: `cycles`, `concurrency`, `rate`, `stanzarate`,
  `stanza_concurrency`, `sc`, `errors`, `seq`, `tags`
- **Workload**: `workload`, `op`, `format`, `filename`
- **Adapter selection**: `adapter`, `driver`
- **CQL**: `hosts`, `host`, `port`, `keyspace`, `consistency`,
  `username`, `password`, `request_timeout_ms`
- **HTTP**: `base_url`, `timeout`
- **Watch**: `watch` — register subprocess-based phase-end
  triggers (see [Watch Triggers](#watch-triggers) below)

Workload params (from `params:` section) are also accepted on
CLI and override YAML defaults.

---

### Watch Triggers

`watch=<spec>[,<spec>...]` registers one or more phase-end
triggers that fire after every successful or failed phase.
Each trigger spawns an `nmbrs` subprocess to re-render a
report or plot against the live session database, so an
external viewer (image viewer, browser, `tail -F`-style
watcher) sees up-to-date output as the run progresses.

Specs:

| Spec form              | Subprocess invoked                            |
|------------------------|-----------------------------------------------|
| `report`               | `nmbrs report all --session <S>`               |
| `report:<args>`        | `nmbrs report <args> --session <S>`            |
| `plot`                 | `nmbrs plot all --session <S>`                 |
| `plot:<name>`          | `nmbrs plot --name <name> --session <S>`       |

`<S>` is the active run's session directory (resolved from
the same `--session=`/`logs/latest` lookup the report
subcommand uses).

Examples:

```
# Re-render every stored plot after each phase end:
nmbrs run workload=fknn.yaml watch=plot

# Re-render one specific plot — point an image viewer at
# logs/latest/throughput.svg and it'll refresh per phase:
nmbrs run workload=fknn.yaml watch=plot:throughput

# Re-render an HTML report:
nmbrs run workload=fknn.yaml watch=report:fmt=html

# Stack triggers — both fire on each phase end, in
# registration order:
nmbrs run workload=fknn.yaml watch=report,plot:recall,plot:throughput
```

**Semantics:**

- Triggers run on a single background worker thread shared
  across all registrations. A slow subprocess won't block
  the executor's next phase, but two triggers can't run in
  parallel — they're sequenced.
- Subprocess `stdout` is suppressed (the re-render's
  side effect is the generated file, not its console text).
  Subprocess `stderr` surfaces as the parent run's WARN log
  on non-zero exit.
- A panic inside a trigger does not stop subsequent triggers
  in the chain — each fire is `catch_unwind`-guarded.
- An unknown spec (e.g. `watch=garbage`) emits a startup
  warning and is skipped; the run continues uninterrupted.
- When no `watch=` is given the registry is empty and the
  worker thread is never spawned — zero overhead.

Implementation: `crates/nmbrs/src/watch_trigger.rs` (subprocess
trigger), `crates/nmbrs-runtime/src/phase_end_triggers.rs`
(content-agnostic registry the executor fires into after
every `phase_completed` / `phase_failed`).

---

## Completions

CLI completions use `veks-completion` as the single source of
truth. The command tree is defined once and generates completions
for bash, zsh, fish, and PowerShell.

```rust
fn cli_tree() -> Tree {
    Tree::new("nmbrs")
        .command("run", Node::leaf_with_flags(
            &["adapter=", "workload=", "cycles=", "concurrency=", ...],
            &["--dry-run"],
        ))
        .command("bench", Node::leaf_with_flags(
            &["cycles=", "concurrency=", "--cycles", "--concurrency"],
            &["--explain"],
        ))
}
```

### Dynamic Completion

Workload params are discoverable: when the user has specified
`workload=file.yaml`, the completion engine parses the YAML
`params:` section and offers those param names as completions.

---

### Plot Command

    nmbrs plot Polydat <expr|file.polydat> [cycles=N] [--width=N] [--height=N]
                                 [--mode=plot|histogram] [--no-color]
                                 [--xscale=N] [--yscale=N] [--max-labels=N]

Evaluate a Polydat expression and render outputs to the terminal:
- Numeric outputs → braille scatter plot (default) or histogram
- String outputs → discrete value histogram
- 24-bit truecolor, auto-detected terminal size
- Auto or manual scale control

---

### Inline Workloads

    nmbrs run op='hello {cycle}'
    nmbrs run op='id={{mod(hash(cycle), 1000)}} name={{number_to_words(cycle)}}'

The `op=` parameter synthesizes a complete workload from a single
template string. `{{expr}}` are inline Polydat expressions compiled
into the kernel. `{name}` are bind point references. Semicolons
separate multiple ops with optional ratio prefixes: `3:read;1:write`.

Single-brace expressions are also supported when the content is
auto-detected as a Polydat expression: `{hashed_uuid(hash(cycle))}`,
`{:=expr}`, `{:=expr:=}`.

`op=` and `workload=` are mutually exclusive; `op=` takes precedence.
Default adapter is `stdout` when `adapter=` is omitted.

---

## --explain Mode

On the `bench` command, `--explain` dumps the Polydat compiler event
stream showing each compilation step:

```
$ nmbrs bench --explain "mod(hash(cycle), 1000)" cycles=5
[parsed]    cycle → graph input #0
[parsed]    hash  → Hash64 node
[wired]     hash.input[0] ← input:cycle
[parsed]    mod   → ModU64(1000)
[wired]     mod.input[0] ← hash.output[0]
[fusion]    mod(hash(x), K) → hash_range(x, K)
[output]    hash_range → selected as program output
[compiled]  1 node (fused), 1 output, 0 constants folded

cycle=0: 723
cycle=1: 456
cycle=2: 891
cycle=3: 234
cycle=4: 567
```

---

## Input Summary

When running `bench`, the CLI prints a summary of all inputs:

```
1 input: cycle (u64)
3 bindings:
  user_id: u64 (dynamic)
  dim: u64 (compile-const, folded to 25)
  query: str (dynamic)
```
