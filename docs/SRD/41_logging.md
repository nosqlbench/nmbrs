# 41: Logging and Diagnostics

Every nmbrs **system signal** — startup banners, the phase walk, metrics
setup, lifecycle readouts, run-completion notices, the post-run summary
— routes through the observer/sink traits. The sink's per-mode wiring
decides the surface (TUI panel, stderr, or `session.log`). The console
is owned by the **adapter**: only adapter output (the stdout adapter's
rendered fields, the plotter's canvas), user-requested report content,
and fatal errors write to it directly.

This is the load-bearing rule. A subsystem that reaches for
`println!` / `eprintln!` to emit a system signal is a bug — it bypasses
the sink, so the line never reaches `session.log`, can't be routed to
the TUI panel, and clobbers a console-owning adapter's output.

---

## Output Routing

### The one rule

| Output | Channel |
|--------|---------|
| Adapter result rendering (stdout fields, plotter canvas) | **stdout, directly** — the adapter owns the console |
| User-requested report content (`summary=`, a rendered `.md`) | **stdout, directly** — it's the asked-for artifact |
| Fatal error just before `process::exit` | **stderr, directly** — terminal reporting must be unconditional, not buffered behind the async sink |
| Every other nmbrs system signal | **the observer/sink** — `diag!` / `observer::log` / the readout binder |

**SRD-87 update:** the *transport* of adapter output is now owned by
[SRD-87](87_output_channel.md). Adapter output no longer writes
"stdout, directly" — it submits the **op-output bucket** of the single
`OutputChannel`, which owns the fd. The intent here ("the console
belongs to the adapter") is preserved as "the adapter's
op-output/raster bucket owns the terminal surface." The interim
`op_output()` half-measure (`crates/nmbrs-runtime/src/observer.rs`) that routed
op output through the diagnostic channel — and produced the
stdout-prints-nothing-on-an-interactive-TTY defect — is superseded by
SRD-87.

`diag!(level, …)` → `observer::log_categorized` is the only legal
in-run output channel for system signals. It writes the line to
`session.log` (always, at the retain level) **and** to the active
sink's surface.

### Severity + category determine the console

The console floor defaults to `Info`; `session.log` retains down to
`Debug`. Lines split along two axes — **level** (what reaches the
console at all) and **category** (which surface, and whether a
console-owning adapter suppresses it):

| Signal | Level | Category | Console (normal) | Console (adapter owns it, §below) |
|--------|-------|----------|------------------|-----------------------------------|
| Startup banners (`N phases`, `session:`, `metrics:`, phase walk), shutdown notices, auto-purge | **Debug** | Diagnostic | hidden (below floor); `loglevel=debug` shows | hidden |
| Run completion (`all phases complete`, `done.`) | Info | **RunLifecycle** | shown | **hidden** |
| Inline `✓` progress / status line | — | (`phase_progress`) | shown | **hidden** |
| Post-run summary (`session:`/`logs:`/`phases:`) | — | (`session_summary` readout) | shown | **hidden** |
| User-bound readouts (`readouts:` block) | Info | Diagnostic / PhaseLifecycle | shown | **shown** |
| Warnings / errors | Warn / Error | Diagnostic | shown | shown |

There is no "SystemBanner" category: banners are demoted by **level**
(Debug) in the existing channels. `RunLifecycle` is a lifecycle marker
(peer to `PhaseLifecycle` / `PhaseOutcome`) so completion notices can be
told apart from a *readout the user deliberately bound* — readouts stay
visible. The categories are owned by [SRD 63](63_status_readouts.md) /
[SRD 81](81_event_sourced_display.md).

### Surface by mode

The sink the observer drives is chosen per `tui=` mode; the *signals*
are identical, only the surface differs:

| Mode | Surface for system signals |
|------|----------------------------|
| `tui=on` | the TUI's log panel (alt-screen) |
| `tui=terminal` (default interactive) | stderr line-mode, via `LogOnlySink` |
| `tui=off` (piped/CI, or a console-owning adapter) | stderr, synchronous |
| *(any)* | `session.log`, always (at the retain level) |

A "no-terminal" run (piped, CI) still routes every signal through the
sink — they reach stderr because the sink is *configured* to write
there, not because of stray prints.

### Console-owning adapters

An adapter that writes its own output to the console declares
[`DisplayPreference::Off`](30_adapter_interface.md) (the stdout adapter
to a terminal; the plotter). That forces the dashboard off — it would
overwrite the adapter's output. On an **interactive TTY** the adapter
owns the whole screen (stdout and stderr both land on it), so the
console is reserved for the adapter: banners are already log-only, and
the `✓` status line, the run-completion notices (`RunLifecycle`), and
the post-run summary are suppressed. User-bound readouts stay visible —
those are output the user asked for. Everything suppressed is in
`session.log`.

This reservation is gated on `is_tty`. A **non-TTY** run (pipe/file)
keeps stdout and stderr as separate streams, so system signals flow to
stderr as usual and CI/tests capture them. `DisplayPreference` is
params-aware: the stdout adapter is `Off` writing to the console
(default / `filename=stdout`) and `Auto` when `filename=` redirects to
a file (the console is free).

The console-owning adapter is detected from the run's **resolved**
adapter, not just the CLI: `run.rs` peeks the workload's declared
`params:` (extends-merged, the same `peek_declared_params` the display
setup uses) so a workload that says `params: { adapter: plotter }` is
recognized as console-owning and quiets the dashboard on a TTY — without
needing `adapter=` repeated on the command line. The workload's own
shaping params (e.g. stdout's `filename`) ride along, so an
adapter-writes-to-a-file workload correctly keeps its dashboard.

### Log-level resolution

The display (stderr) and retain (`session.log`) floors resolve with
**closest-wins precedence: CLI > workload `params:` > `NMBRS_LOG_*` env >
built-in default** (display `Info`, retain `Debug`). So a visual or
quiet-by-default workload can declare `params: { loglevel: warn }` and a
CLI `loglevel=info` still overrides it. Knobs: `loglevel=` /
`loglevel-display=` / `NMBRS_LOG_DISPLAY_LEVEL` (display) and
`loglevel-retain=` / `NMBRS_LOG_RETAIN_LEVEL` (file). All are accepted
workload/CLI params (in `KNOWN_PARAMS`), so they pass the closed-vocabulary
guard.

### Conventions

- Prefix with subsystem: `vectordata:`, `metrics:`, `validation:`.
- The `diag!` level conveys severity — don't also prefix `warning:` /
  `error:` in the message body for `Warn` / `Error` lines.
- No timestamps in console messages; the `session.log` projection adds
  a wall-clock stamp, and metrics carry timing.
- The inspector socket (`nmbrs attach`'s out-of-band endpoint) is **off
  by default**; opt in with `inspector=on`. It is not part of the
  in-process display path — the in-process TUI/observer never read it.

---

## Polydat Compiler Diagnostics

The compiler emits structured `CompileEvent` values explaining
each compilation step:

```rust
pub enum CompileEvent {
    Parsed { node: String, function: String },
    BindingResolved { name: String, source: String },
    ModuleInlined { module: String, prefix: String },
    TypeAdapterInserted { from: String, to: String, node: String },
    ConstantFolded { node: String, value: String },
    FusionApplied { pattern: String, nodes: Vec<String> },
    CompileLevelSelected { node: String, level: String },
    OutputSelected { name: String, consumers: Vec<String> },
}
```

### --explain Mode

`nmbrs bench --explain <expr>` dumps the event stream to stderr:

```
$ nmbrs bench --explain "hash(cycle)" cycles=1
[parsed]    cycle → graph input #0
[parsed]    hash  → Hash64 node
[wired]     hash.input[0] ← input:cycle
[output]    hash  → selected as program output
[compiled]  1 node, 1 output, 0 constants folded
```

Shows parsing, wiring, type adaptation, constant folding, fusion,
and output selection decisions.

### Polydat Compiler Events

| Event | Level | Description |
|-------|-------|-------------|
| Parsed | Info | AST created |
| BindingResolved | Info | Name → node |
| ModuleInlined | Info | Module expanded |
| TypeAdapterInserted | Advisory | Auto-coercion |
| TypeWidening | Advisory | u64→f64 promotion |
| ConstantFolded | Info | Init-time eval |
| FusionApplied | Info | DAG rewrite |
| ConfigWireCycleWarning | Warning | Config wire perf |
| Warning | Warning | General |

Query advisories: `nmbrs bench Polydat file.gk --explain`

---

## Validation Diagnostics

### End-of-Run Summary

When validation is active, a summary prints after all fibers
complete:

```
  recall@100: mean=0.9385 p50=0.9503 p99=1.0000 min=0.7800 max=1.0000 (n=100)
  precision@100: mean=0.9385 ...
validation: 100 passed, 0 failed
```

### Hard Errors

Missing ground truth, empty result extraction, and similar
validation setup problems are hard errors (not silent zeros):

```
error: [op] [relevancy_error] relevancy: no ground truth for
'ground_truth'. Available fields: ["prepared"].
```

### Extraction Warnings

First occurrence of empty result extraction logs a warning with
a result preview:

```
warning: relevancy: no values extracted for field 'key' from result
  result preview: [{"key":"abc",...}]
```

---

## Error Router Logging

The error router controls which errors produce log output:

- `warn` action: logs to stderr with error name, message, cycle
- `ignore` action: suppresses logging (error still counted)
- `stop` action: logs and halts activity
- All errors always counted regardless of logging config
