# SRD-87: Output Channel — the single typed terminal-output conduit

> **Status:** DRAFT — model agreed (2026-06-20). Names the single
> **`OutputChannel`** transport seam: every byte that reaches the one
> user terminal passes through it, as a typed **bucket** submission,
> and it is the **sole owner of fd writes**. It consolidates the
> output halves of `RunObserver`, `DisplaySink`, and the free
> `op_output()` into one swappable surface, generalizing the Readout
> system's notify / observe / project template to *all* output.
> Supersedes the **transport** halves of SRD-41/30/52; **preserves**
> SRD-02's actor+ArcSwap fold and SRD-63/81 as the content/spine
> layers that sit *above* this seam.
>
> **As-built (2026-06-20):** Pushes 1 and 2 landed — the trait, the
> test-capture impl, the **op-output** bucket (fixing the §2 stdout
> defect and reverting the global-flag prototype), the **status** bucket,
> and the **log** bucket; producers (the activity status line, the
> observer's synchronous log writes) submit through the channel; the
> behavioral harness (`crates/nmbrs/tests/output_channel_harness.rs`) pins all
> three contexts. **Stage C reframed:** reading the code showed
> `DisplaySink` is a *pull* fold-drainer (per its own trait doc — "events
> flow through the actor; the sink drains"), so the original "make the
> sinks *be* `OutputChannel` impls" was a misframe. The SRD-81 fold model
> (producers → fold → sink drains) is the correct structure; the
> producer-side consolidation (pushes 1–2) is its substance. The sinks
> stay drainers; their terminal writes are the interactive surface's
> legitimate raster realization, and `set_status_line` is a legitimate
> fold-write API, not "forwarding to retire." See §11 step 1.

## 1. Ownership & relationships

This SRD owns **how produced bytes reach the one user terminal** — the
transport contract, not the content (readouts) and not the events
(spine). It sits across:

- [SRD-63 readouts](63_status_readouts.md) — owns *what* content a
  readout renders and the `ReadoutSink` projection. A `ReadoutSink`
  is one **producer** that feeds this channel's `status`/`log` buckets.
- [SRD-81 event-sourced display](81_event_sourced_display.md) — owns
  the typed event **spine**, the `RunState` fold, and the durable
  session store. This SRD is the **project → surface** transport that
  SRD-81's third arrow flows through.
- [SRD-41 logging](41_logging.md) — its "one rule" routing table is
  **superseded here**: adapter output no longer writes "stdout
  directly"; it submits the **op-output bucket** and *this channel*
  owns the fd. The intent ("the console belongs to the adapter") is
  preserved as "the adapter's op-output/raster bucket owns the
  terminal surface."
- [SRD-30 adapter interface](30_adapter_interface.md) — `DisplayPreference`
  / console-ownership is reframed: a console-owning adapter is one
  whose bucket claims the terminal surface for the run.
- [SRD-52 stdout model](52_stdout_model.md) — the stdout adapter is the
  canonical op-output producer; this SRD owns where its bytes go.
- [SRD-02 concurrency](02_concurrency_model.md) — the actor + ArcSwap
  decoupling is **unchanged**; this channel is the non-blocking
  transport beneath it.

The forcing question: **can every user-visible byte be expressed as a
typed submission to one swappable conduit that alone touches the
terminal — so no producer ever writes an fd, branches on `is_terminal()`,
or contends for the screen?** This SRD says yes, and names the seam.

## 2. The defect this fixes

`nmbrs run op='id-{cycle}' adapter=stdout` prints **nothing** on an
interactive terminal (it works when piped). Root cause: the stdout
adapter's op output is routed through `op_output()` → `log()` — the
*diagnostic* channel — and a console-owning run raises `sink_active`
to keep diagnostics off the console, which then swallows the adapter's
own output along with them. The plotter dodges this by writing raw to
stdout itself; so two console-owning adapters have **two different,
ad-hoc** output paths, and one of them is broken.

The deeper defect is **plane conflation**: op-output (data the user
asked for) shares a transport with diagnostics (the run's narration),
and `is_terminal()` is used as a proxy for "a display is compositing
the screen" — a proxy that is wrong precisely when a console-owning
adapter owns the screen with no display active. A point-fix
(a `console_reserved_for_adapter` global consulted inside `op_output`)
was prototyped and is **explicitly reverted** by this SRD (§11): the
correct shape is one conduit with a typed op-output bucket, not another
proxy taught about one more case.

## 3. The three-layer model

```
 L1  EVENT SPINE ── intake ─▶ RunState fold (ArcSwap)         [BUILT]
     (lifecycle +          └▶ durable record (sqlite +
      diagnostics)            checkpoint JSONL + session.log)

 L2  CONTENT PRODUCERS                                         [BUILT]
     ├ ReadoutSink / ReadoutBinder   (readout → string/spans)
     ├ compositor                    (reads fold → screen frames)
     └ adapters                      (op-output strings / raster frames)

 L3  OUTPUT CHANNEL ── typed buckets, sole fd owner,          [THIS SRD]
     non-blocking, swappable impl ──▶ the one user terminal
```

- **L1 (spine)** is the source of truth: typed `lifecycle::EventType`
  facts fold into `RunState` (live) and persist to the session store
  (durable, *including* `session.log` via the async log sink). Owned by
  SRD-81.
- **L2 (producers)** turn events/fold/adapter results into **bytes**:
  `ReadoutSink` renders readout content per surface; the compositor
  reads the fold and paints screen regions; adapters emit op-output
  strings and raster frames. Owned by SRD-63/30/52.
- **L3 (this channel)** is the only thing that touches the terminal.
  Producers **submit to a bucket**; the channel buffers, composes, and
  writes the fd.

The architecture is already ~80% here: the readout layer never touches
an fd (it hands content to the observer), and almost every real fd
write already lives inside a `DisplaySink` impl. This SRD names the
seam, folds in the two stragglers (`op_output`, the plotter's raw
paint), and removes the plane conflation.

## 4. The `OutputChannel` trait

`OutputChannel` generalizes the **three primitives** the Readout system
already proves out, applied to *all* output:

1. **Event-notification (callbacks).** Producers *notify*; downstream
   never blind-polls. The readout `EventType` callbacks
   (`on_phase_end`, `on_update`, …) are the model. The **log** bucket
   gets a callback too, **rate-throttled** so a chatty run does not
   fire one per line.
2. **Auxiliary state observation (context facade).** A notified surface
   *reads what it needs* on demand — the `ReadoutContext` model — rather
   than having every datum pushed at it.
3. **Per-surface projection (sink).** Each impl renders each bucket in
   its surface's native form — the `ReadoutSink` model, lifted to every
   bucket.

Shape (illustrative — finalized in implementation):

```rust
/// The single conduit for the one user terminal. Exactly one impl is
/// installed per run. Producers submit to a bucket; only the impl
/// writes an fd. Every method is non-blocking.
pub trait OutputChannel: Send + Sync {
    /// op-output: an adapter's rendered op (string form). Reaches the
    /// terminal/file the channel owns — never around it.
    fn op_output(&self, line: &str);

    /// raster: a composited screen-region frame (braille, ANSI cells).
    /// The producer owns layout; the channel owns the fd + buffering.
    fn raster(&self, frame: RasterFrame);

    /// log: a structured diagnostic event for the *live* surface.
    /// (The durable session.log write is L1, not this — see §9.)
    /// Coalesced/throttled per §7.
    fn log(&self, event: &LogEvent);

    /// status: the live, rewritable status line (readout-projected).
    fn status(&self, rendered: Option<StatusFrame>);

    /// Cooperative flush + terminal restore. Idempotent.
    fn shutdown(&self);
}
```

The bucket set is **open** ("probably more" — §5 lists the four known
today); a new output *kind* is a new bucket, not a new fd write.

## 5. Buckets

Each bucket is a semantic/format/content category. The trait *as a
whole* is "the channel"; **producers must submit to the correct
bucket.**

| Bucket | Content | Producer | Readout compositor? | Ordering |
|---|---|---|---|---|
| **op-output** | rendered op, string | stdout / testkit adapter | No — direct | presentation order |
| **raster** | braille / ANSI screen-region frame | plotter; or compositor reading the fold | No — direct | presentation order |
| **log** | structured diagnostic event | `diag!` → throttled callback + windowed ring | partial (a log *view* may use readouts) | **timestamped + serialized** |
| **status** | live rewritable status line | `phase_status` readout via the binder | Yes — full compositor | latest-wins (single line) |

Two consequences, both from your guidance:

- **The readout compositor is not always mounted.** Plotter braille and
  stdout text submit straight into `raster`/`op-output` — through the
  channel traits, but with no binder / `ReadoutContext` in the path.
  The compositor is *one* producer kind, mounted only for the
  readout-backed buckets (`status`, and log *views*).
- **Only the log bucket is timestamped and strictly serialized.** Every
  other bucket merely **preserves presentation order** (the order the
  producer submitted). There is one user terminal and many sources;
  the channel composes them, but promises cross-bucket *causal*
  serialization only for log.

## 6. Contract (axioms)

These are load-bearing; a proposal may not contradict them.

- **A1 — Sole fd owner.** No code outside the installed `OutputChannel`
  impl writes stdout/stderr/the terminal. Every `println!` /
  `eprintln!` / `print!` / raw `io::stdout()` / crossterm `execute!`
  for *user output* is a bug. (Carve-out: pre-channel bootstrap and
  fatal-exit errors — §6 carve-out.)
- **A2 — Non-blocking.** Submission never stalls a producer. The impl
  buffers (bounded) and drops-with-count on overflow, the `log_sink`
  discipline (SRD-02). No producer ever waits on the terminal.
- **A3 — Swappable, selected once.** The impl is chosen once per run
  for the context (§10). Producers never branch on `is_terminal()` —
  the impl encodes that decision, once.
- **A4 — Per-bucket ordering.** Log is timestamped + serialized; all
  other buckets preserve presentation order; no cross-bucket causal
  promise beyond that.
- **A5 — Notify, don't poll.** Downstream is driven by callbacks
  (§4.1); log callbacks are throttled (§7). The interactive compositor
  reads the fold on notification, not on a blind timer.
- **A6 — The terminal only.** `OutputChannel` governs the one user
  terminal. Durable artifacts (`session.log`, the session store) are
  L1 and are written independently (§9).

**Carve-out.** Two classes may write an fd directly: (a) **bootstrap**
errors before the channel is installed (no surface exists yet);
(b) the **fatal-exit** report immediately before `process::exit`
(terminal reporting must be unconditional, not buffered behind an async
sink). Both are stderr-only and must be unmistakably terminal.

## 7. Consumption patterns

Two patterns, both notification-driven (no blind poll):

- **Notified-render.** A callback fires; the surface renders once.
  Used for `status`, lifecycle, and phase outcomes — the readout
  `on_*` fires, the sink projects, done.
- **Notified + windowed-read.** A **throttled** "more is available"
  signal; the surface then **reads while there is more, tracking its
  window cursor**, draining at its own pace. Used for log views (and
  potentially long op-output streams). This is exactly today's ring +
  `resume_from` / `log_seq_total` mechanism (the 50 ms poll becomes a
  throttled callback): the ring is the window, each surface holds a
  read cursor, and a high-rate producer coalesces notifications rather
  than waking the renderer per line.

## 8. Producers

- **`ReadoutSink` + `ReadoutBinder`** (SRD-63) feed `status` and log
  *views*: the binder fires the bound readouts for an event, the sink
  renders content, the content is submitted to the bucket. Already
  built; already fd-free.
- **The compositor** (`DisplaySink`: `LogOnlySink`/`TuiSink`) is the
  interactive surface's **`raster` realization**: it *drains* the
  `RunState` fold (pull, per its trait contract — "events flow through
  the actor; the sink drains") and paints the terminal it owns. It is
  **not** inverted into a push-receiver; it owns the terminal fd as the
  interactive context's raster, and the status / log / op-output buckets
  reach it via the fold it drains.
- **Adapters** submit `op-output` (stdout/testkit) and `raster`
  (plotter) directly — no compositor.

A producer never owns an fd; it owns *bytes* and a bucket.

## 9. `session.log` is not a bucket (decision A)

A single log event fans out to **two independent targets**:

1. the **L1 durable spine** — appended to the in-memory ring and
   written to `session.log` by the existing non-blocking async log
   sink, *regardless of which surface is up*; and
2. the **L3 `OutputChannel` log bucket** — projected to the *live*
   terminal.

`session.log` is **not** an `OutputChannel` surface. The "everything
through the trait" rule governs the **one user terminal**; the durable
file is a record, not the terminal (axiom A6). Keeping it on the L1
async path preserves SRD-02's decoupling and means a dropped/late
terminal frame never costs a durable line.

## 10. Implementation set & selection

One impl is installed per run, chosen by `run.rs` from the existing
signals (`tui_mode`, `is_tty`, console-owning adapter, dryrun):

| Impl | Context | Buckets |
|---|---|---|
| **interactive-terminal** | TTY, `tui=terminal`/`on` | compositor owns `raster`; `status`/`log` composited; `op-output` interleaves |
| **console-owning-adapter** | TTY, adapter `DisplayPreference::Off` | adapter owns `op-output`/`raster`; `log`/`status` go log-only (L1), not the terminal |
| **piped** | non-TTY (pipe/CI) | `op-output` → stdout; `log` → stderr; no `raster`/`status` redraw |
| **headless** | no terminal (daemon) | terminal buckets dropped; L1 still durable |
| **test-capture** | tests | every bucket captured in-memory; asserts no fd bypass |

The console-owning vs interactive split is exactly today's
`silent_console` decision — but it now selects an **impl**, instead of
raising a flag that a producer's `op_output` must remember to consult.
The stdout bug (§2) becomes impossible: under the console-owning impl,
`op-output` *is* the terminal surface.

## 11. Migration

Sequenced so each step builds and is independently verifiable.

1. **Producer-side consolidation (pushes 1–2, landed).** Producers
   submit through `OutputChannel` instead of writing fds or routing
   around it: the stdout adapter's op-output (the **op-output** bucket),
   the activity inline-status line (the **status** bucket), and the
   observer's synchronous log writes (the **log** bucket — the channel
   owns that stderr fd). The `sink_active`/`min_level` **gate** and the
   durable `session.log`/fold intake stay in the spine (L1).
   **The `DisplaySink` impls are NOT inverted.** Reading the code
   (`display_sink.rs`: "events flow through the actor; the sink drains";
   `set_status_line` → `state.send(SetStatusLine)`) shows the sinks are
   *pull* fold-drainers — the correct SRD-81 surface. They stay drainers;
   their terminal writes are the interactive context's legitimate
   `raster` realization (the interactive impl owns that fd by *being* the
   compositor). `set_status_line` is the status bucket's **fold-write
   API** (the channel pushes the line into the fold; the sink drains it),
   not "forwarding to retire."
2. **`op_output()` → the op-output bucket.** Delete the free function's
   `is_terminal()` proxy. **Revert** the `console_reserved_for_adapter`
   global prototype (observer.rs) and the `set_console_reserved_for_adapter`
   call in `run.rs` — the console-owning *impl* replaces it.
3. **Plotter raw paint → the raster bucket.** The plotter stops calling
   `print!`/flush; it submits `RasterFrame`s. It is no longer a special
   case — same story as stdout (both just submit a bucket).
4. **Sweep the stragglers.** `DIRTY:`/`DBG:` debug `eprintln!`s
   (wires.rs, synthesis.rs) → the trace router; audit the remaining
   `println!`/`eprintln!` sites against A1 (the bootstrap/fatal-exit
   carve-out is the only legitimate residue; CLI subcommands —
   `describe`/`metrics`/`report`/`replay` — are out of scope: they are
   one-shot tools, not run output).
5. **session.log stays L1** (no change beyond confirming it is not
   routed through a bucket).

## 12. Load-bearing test

Two properties, modeled on SRD-47's reducer-equivalence and SRD-81's
sink-agreement:

- **No-bypass.** Under the `test-capture` impl, a representative run
  produces **zero** direct fd writes outside the channel (enforced by a
  capture harness + a grep gate in CI for new `println!`/`eprintln!` in
  the runtime/tui/adapter output paths). This is the A1 gate.
- **Surface-agreement for op-output.** The op-output bucket's bytes are
  **identical** across the console-owning impl and the piped impl for
  the same workload (the bug §2 is exactly this property failing). This
  pins "the adapter's product is the same regardless of surface."

## 13. Push sequence

1. **Trait + test-capture impl + the op-output bucket.** Land
   `OutputChannel`, the capture harness, and route the stdout adapter's
   op-output through it. Revert the global-flag prototype. The §2 bug is
   fixed and the surface-agreement test passes. (First shippable slice;
   smallest diff that closes the defect the right way.)
2. **Status + log buckets, producer-side (landed; reframed).** Route the
   activity status line through the `status` bucket and the observer's
   synchronous log writes through the `log` bucket (the channel owns the
   log stderr fd); the `sink_active`/`min_level` gate and the
   `session.log`/fold intake stay in the spine. **No sink inversion** —
   the `DisplaySink`s stay fold-drainers (§11 step 1, reframed). The
   behavioral harness pins piped / console-owning / interactive.
3. **Plotter → raster bucket**; retire the plotter's raw paint and its
   deadlock-prone shutdown path entirely.
4. **Straggler sweep + A1 CI gate (landed).** The hot-path stragglers
   route through the log channel — the plotter's render-rate warning and
   the testkit `diagnose=` diagnostics now `nmbrs_runtime::diag!` instead
   of raw `eprintln!`. The **A1 no-bypass gate** is
   `crates/nmbrs/tests/architecture_rules.rs::a1_output_channel_no_fd_bypass`: it
   asserts the op-output / raster / readout *producer* paths
   (`adapters/{stdout,plotter,testkit}`, `crates/nmbrs-runtime/src/readouts`)
   contain zero raw terminal-write macros, proven to fail on an injected
   write. Scope is the producers that race the live display, NOT every
   `eprintln!` — the carve-outs (the channel itself, bootstrap/session
   errors, the post-run summary after teardown, CLI subcommands) are out
   of scope per §6/§11. The gated `nmbrs_dirty_debug_enabled()`
   `DIRTY:`/`DBG:` dev instrumentation (wires.rs / synthesis.rs) is
   dev-only, off the display path, and left as an env-gated carve-out
   rather than forced through the trace router.

## 14. References

- SRD-63 §1/§5/§7 — the Readout / `ReadoutSink` / `ReadoutBinder`
  template this channel generalizes.
- SRD-81 §3/§6 — the spine/fold/project model; this channel is the
  project→surface transport.
- SRD-41 §"Output Routing" — the routing table whose **transport** half
  this supersedes (adapter output → op-output bucket, not "stdout
  directly").
- SRD-30 §`DisplayPreference` — console-ownership, reframed as
  bucket-owns-surface.
- SRD-02 §"Display and Diagnostic Decoupling" — the actor+ArcSwap +
  async log sink discipline this channel preserves.
- `feedback_no_blocking_in_async`, `feedback_lock_free_metrics`,
  `feedback_display_actor_decoupling` — the non-blocking / lock-free /
  actor posture axioms A2/A5 inherit.
