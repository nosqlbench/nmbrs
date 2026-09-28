# Design: Live Status Display (ratatui)

## Problem

The current status line is a single `\r`-overwritten line on stderr:

```
fknn_rampup_data (optimize_for=RECALL) cursor=row (pending,active,complete)=(922537,100,77463) 7.75% 208/s ok:100.0% errors:0 retries:0 fibers:100 rows_inserted:1.7K/s rows/batc
```

This truncates at terminal width, mixes with compiler warnings and
CQL driver log messages, and can't show multiple concurrent phases,
latency distributions, or time-series trends.

## Design

A full-terminal ratatui display that replaces the single status line
during workload execution. Activates automatically when stderr is a
TTY and no `dryrun=` mode is active. Can be disabled with `tui=off`.

### Layout

```
┌─ nmbrs ─────────────────────────────────────────────────────────────────────┐
│ workload: full_cql_vector.yaml  scenario: fknn_rampup  elapsed: 5m 23s    │
│ adapter: cql (127.0.0.1)       profiler: off           limit: none        │
├─ Phase ────────────────────────────────────────────────────────────────────┤
│ ▶ fknn_rampup_data             cursor: row   0━━━━━━━━━━━━━━━━━━━━━━━━ 1M │
│   fibers: 100  active: 100     ops/s: 233    rows/s: 1.8K   rows/batch: 8 │
│   ok: 100.0%   errors: 0       retries: 0    pending: 766K   done: 234K   │
├─ Latency (service time) ──────────────────────────────────────────────────┤
│   p50    1.2ms  ▏██████████████████████████                               │
│   p90    3.8ms  ▏████████████████████████████████████████████████████████  │
│   p99   12.4ms  ▏████████████████████████████████████████████████████████▎ │
│   p999  45.1ms  ▏█████████████████████████████████████████████████████████ │
│   max   89.2ms                                                            │
├─ Throughput ──────────────────────────────────────────────────────────────┤
│   ops/s  ▁▂▃▄▅▆▇█████████████████████████████████████████████████▇▇▇██  │
│   rows/s ▁▂▃▅▆▇████████████████████████████████████████████████████████  │
├─ Scenario Tree ───────────────────────────────────────────────────────────┤
│   ✓ teardown          3 ops    0.2s                                       │
│   ✓ schema            4 ops    1.1s                                       │
│   ▶ fknn_rampup_data  1 op    [5m 23s ...]                                │
│   ○ pvs_query (k=10)                                                      │
│   ○ pvs_query (k=100)                                                     │
├───────────────────────────────────────────────────────────────────────────┤
│ q: quit   p: pause/resume   d: detail toggle   ?: help                    │
└───────────────────────────────────────────────────────────────────────────┘
```

### Sections

#### 1. Header (2 lines, fixed)

Workload file, scenario name, adapter target, elapsed time,
profiler status, limit setting. Static for the run, except elapsed.

#### 2. Phase Panel (3 lines, updates at 4 Hz)

The currently executing phase. Shows:

- **Phase name** with `▶` active indicator
- **Cursor**: name and progress bar with extent
- **Fiber count** and **active count** — instantaneous
- **Throughput**: ops/s and adapter-specific rates (rows/s)
- **rows/batch**: average batch fill ratio
- **Health**: ok%, errors, retries
- **Pending/done** counts

Color coding:
- Active ops count: gradient from green (low) to amber (saturated)
- ok%: green at 100%, red gradient below
- errors: red if > 0
- Progress bar: 24-bit gradient from `#2d5a27` (dark green) to
  `#7ac142` (bright green) as completion progresses

#### 3. Latency Panel (5 lines, updates at 2 Hz)

HDR histogram percentiles from the last capture interval.
Horizontal bars show relative magnitude (log scale, max = p999).

Color gradient on bars:
- p50: `#4dc9f6` (cool blue)
- p90: `#f7c948` (amber)
- p99: `#f77f00` (orange)
- p999: `#d62828` (red)

Values auto-scale units (ns → µs → ms → s).

#### 4. Throughput Sparklines (2 lines, scrolling)

60-column sparklines (one character = one capture interval) for
ops/s and adapter-specific rates. Uses Unicode block elements
`▁▂▃▄▅▆▇█` for 8-level resolution per column.

Color: 24-bit gradient based on throughput relative to peak.

#### 5. Scenario Tree (variable height, fills remaining space)

Shows the full scenario tree with completion status:

- `✓` completed (green)
- `▶` running (bright white, bold)
- `○` pending (dim gray)
- `✗` failed (red)

Each completed phase shows op count and duration. Running phase
shows elapsed time with `...` indicator. Nested `for_each` and
`for_combinations` show iteration progress.

#### 6. Footer (1 line, fixed)

Keybindings. Minimal.

### Data Flow

```
Activity metrics ──► mpsc::channel ──► TUI event loop
  (ops_started,        (MetricsFrame     (drain channel,
   ops_finished,        every 250ms)       update state,
   service_time,                           render frame)
   adapter counters)

Scenario tree ──► Arc<RwLock<TreeState>> ──► TUI reads
  (phase start/end,
   for_each progress)
```

The TUI runs on its own thread (not a tokio task) to avoid
blocking the async runtime. It reads from two sources:

1. **MetricsFrame channel** — same as the current scheduler
   reporter interface. The TUI registers as a reporter at the
   base cadence (1s) and interpolates between frames for smooth
   display.

2. **Scenario tree state** — a shared `Arc<RwLock<TreeState>>`
   that the executor updates as phases start/complete. The TUI
   reads this to render the scenario tree panel.

### Activation

```rust
// In runner.rs, after scenario tree is resolved:
let tui_mode = merged_params.get("tui")
    .map(|s| s.as_str())
    .unwrap_or(if is_tty { "on" } else { "off" });

match tui_mode {
    "on" => {
        // Enter alternate screen, start TUI event loop
        let (tx, rx) = mpsc::channel();
        sched_builder = sched_builder.add_reporter(
            Duration::from_millis(250), TuiReporter(tx));
        let tree_state = Arc::new(RwLock::new(TreeState::new(&scenario_nodes)));
        let tui_handle = std::thread::spawn(move || {
            App::with_metrics(rx, tree_state).run()
        });
        // ... execute scenario ...
        tui_handle.join();
    }
    _ => {
        // Current behavior: single-line progress on stderr
    }
}
```

### Dependencies

```toml
[dependencies]
ratatui = { version = "0.29", features = ["all-widgets"] }
crossterm = "0.28"
```

No additional crates needed. ratatui supports 24-bit color natively
via `Color::Rgb(r, g, b)`.

### Keybindings

| Key | Action |
|-----|--------|
| `q` / `Esc` | Quit (sends stop signal, waits for current phase) |
| `p` | Pause/resume execution (toggle stop flag) |
| `d` | Toggle detail level (compact / expanded latency) |
| `l` | Toggle log panel (show last N stderr lines) |
| `?` | Show help overlay |

### Graceful Degradation

- **Non-TTY** (pipe, redirect): no TUI, no status line. Just
  phase start/complete messages.
- **Narrow terminal** (< 80 cols): compact mode, sparklines
  hidden, latency panel shows numbers only.
- **`tui=off`**: explicit disable, falls back to single-line
  progress.
- **Short phases** (< 2s): skip TUI for that phase, print
  summary line instead.

### Color Palette (24-bit)

```
Background:  #1a1a2e (dark navy) — alternate screen only
Borders:     #3a3a5c (muted purple-gray)
Text:        #e0e0e0 (light gray)
Emphasis:    #ffffff (white, bold)
Phase name:  #7ac142 (green) active, #808080 (gray) pending
Progress:    #2d5a27 → #7ac142 gradient
OK badge:    #4caf50 (green)
Error badge: #f44336 (red)
Latency p50: #4dc9f6 (blue)
Latency p90: #f7c948 (amber)
Latency p99: #f77f00 (orange)
Latency max: #d62828 (red)
Sparkline:   #4dc9f6 → #7ac142 gradient
Dim text:    #606060 (used for pending phases, help text)
```

### What Changes

| Component | Change |
|-----------|--------|
| `crates/nmbrs-tui/src/app.rs` | Replace current layout with the new design |
| `crates/nmbrs-tui/src/widgets.rs` | `MetricsState` gains phase info, cursor, adapter counters, sparkline history |
| `crates/nmbrs-tui/src/tree.rs` | NEW — `TreeState` struct, scenario tree rendering |
| `crates/nmbrs-tui/src/sparkline.rs` | NEW — Rolling sparkline widget with 24-bit gradient |
| `crates/nmbrs-tui/src/latency.rs` | NEW — Horizontal bar chart for percentiles |
| `crates/nmbrs-runtime/src/executor.rs` | Update `TreeState` on phase start/complete |
| `crates/nmbrs-runtime/src/activity.rs` | Remove single-line progress thread when TUI is active |
| `crates/nmbrs-runtime/src/runner.rs` | TUI activation logic, reporter registration |

### Not Changing

- The `MetricsFrame` format — TUI consumes the same frames as SQLite
- The scheduler cadence — TUI interpolates for smooth display
- The `profiler=` parameter — orthogonal to display
- Log output — suppressed in TUI mode, captured to a ring buffer
  accessible via `l` key
