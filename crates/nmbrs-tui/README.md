# nmbrs-tui

`nmbrs-tui` is the terminal display layer of [nmbrs](https://crates.io/crates/nmbrs).
It provides the full-screen [ratatui](https://crates.io/crates/ratatui)
dashboard (scenario tree, per-phase progress, throughput sparklines, latency
percentile histories, log panel, help overlay), the line-mode status display
used by default in an interactive terminal, and the introspection socket
behind `nmbrs attach`. It is written for the nmbrs binary; its API follows the
nmbrs runtime's observer and metrics interfaces and is not a general-purpose
TUI toolkit.

## Where it sits in nmbrs

The [nmbrs](https://crates.io/crates/nmbrs) binary uses this crate for every
live display mode of `nmbrs run` and for `nmbrs attach`. It depends on
[nmbrs-runtime](https://crates.io/crates/nmbrs-runtime) (the run observer
interface, scene tree, dynamic controls),
[nmbrs-metrics](https://crates.io/crates/nmbrs-metrics) (metric snapshots and
the reporter trait) and [polydat](https://crates.io/crates/polydat).

## Display modes in `nmbrs run`

The `tui=` parameter of `nmbrs run` picks the display; each mode is backed by
this crate:

| Mode | When | Backed by |
|------|------|-----------|
| `terminal` | Default when stderr is a terminal. Line-mode status; Ctrl-T switches to the full-screen dashboard and back | `log_only_observer::LogOnlyObserver`, `log_only_sink`, `sink_supervisor::SinkSupervisor` |
| `on` | Opt-in full-screen dashboard | `observer::TuiObserver`, `app::App` |
| `formatted` | Default when stderr is not a terminal. Append-only status lines, no cursor control | `LogOnlyObserver`, `formatted_line_sink::FormattedLineSink` |
| `off` | Plain log output; forced for adapters that draw on the terminal themselves, such as `plotter` | `LogOnlyObserver`, no sink |

```
nmbrs run workload=selfcheck tui=on
```

In the full-screen dashboard, `?` toggles the help overlay listing every key.
Among them: `l` toggles the log panel, `p` freezes the display on the current
snapshot, and `e` opens a prompt to set a dynamic control value.

## Architecture

- `state::RunState`: the display model. Holds the scene tree, per-phase
  summaries, active-phase counters, the log ring and the sparkline and
  percentile histories.
- `run_state_actor::spawn_run_state_actor(RunState)` returns a
  `RunStateHandle`. The actor thread owns the `RunState`; producers change it
  by sending `RunStateCmd` messages (`RunStateHandle::send`), and readers get
  an immutable `Arc<RunState>` snapshot with `RunStateHandle::load`, a single
  atomic load. No display code takes a lock shared with the executor.
- `observer::TuiObserver::new(handle, cadences)`: the runtime observer that
  turns run lifecycle events into `RunStateCmd`s. `observer::print_post_run_summary`
  prints the summary after the display is torn down.
- `app::App::new(frame_rx, run_state, metrics_query)` and `App::run()`: the
  full-screen render loop, run on a dedicated OS thread (default tick 250 ms).
- `reporter::TuiReporter::channel()`: a `nmbrs_metrics::scheduler::Reporter`
  that forwards metric snapshots to the dashboard over an mpsc channel.
- `display_sink::DisplaySink`: the trait implemented by live-stream consumers
  of the `RunState` snapshots, managed by `sink_supervisor::SinkSupervisor`.
- `inspector_server` / `inspector_repl`: a Unix-domain-socket introspection
  endpoint, started when a run is given `inspector=on`, and the client used
  by `nmbrs attach` (`discover_sockets`, `query`, `run_repl`). The server
  runs on its own OS thread and reads snapshots only, so it keeps answering
  while the executor is stalled.

## Links

- Repository: https://github.com/nosqlbench/nmbrs
- Crate source: https://github.com/nosqlbench/nmbrs/tree/main/nmbrs-tui
- TUI contract: https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/59_tui_contract.md
- TUI layout: https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/62_tui_layout.md
- API docs: https://docs.rs/nmbrs-tui

## License

Apache-2.0
