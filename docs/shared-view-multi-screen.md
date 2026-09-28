# Shared view: one renderer, many screens

*Status: designed, not implemented. Paused 2026-07-27.*

## The ask

The console view you get from running `nmbrs` directly should be **one of several
screens rendered the same way** — a second terminal (or a browser) attached to a
running session should see the same thing, without mirroring the tty.

## What already exists (the hard part is done)

- **Multi-client transport.** `crates/nmbrs-tui/src/inspector_server.rs` listens on a
  Unix socket at `${XDG_RUNTIME_DIR:-/tmp}/nmbrs-<pid>.sock` and its accept loop
  spawns **a thread per connection**, so concurrent clients already work. Each
  connection is one request/response, stateless.
- **Wedge-proof reads.** The endpoint reads through
  `RunStateHandle::load()` — a single atomic load of an `Arc<RunState>` — which
  is why introspection stays responsive when tokio is stuck. This is the
  property that makes a second screen safe: observers can never block the run.
- **The console view is already snapshot-driven.** `log_only_sink` renders from
  `state.load()` plus a drained log tail. **There is no hidden tty state to
  share** — the view is a function of the snapshot, which is exactly what makes
  this tractable.
- **`nmbrs attach`** exists (with a `--tui` flag), and **`nmbrs web`** exists as a
  daemon.

## Why the views differ today

The renderer is not shared. `attach` speaks a text query protocol (`phases`,
`tree`, `metric`, `readout`, `snapshot`, …) and formats its own output; the
console path runs `status_fold` → painter. Two renderers, therefore two views.
Nothing structural requires this — `attach` was built as a diagnostic REPL, not
as a second screen.

## Design

Make rendering a pure function of a snapshot, with the local TTY as one consumer
among several:

```
   RunState snapshot (+ log tail) ──► status_fold ──► rows ──► painter ──► frame
            ▲                    ▲
       in-process            over socket
       (local tty)        (attach --tui, web, …)
```

Work items:

1. **Serialize the snapshot.** `RunState` is `#[derive(Clone)]` only. Add serde
   to it and to `SceneTree`, `PhaseSummary`, `SceneNodeId`. One wrinkle:
   `started_at: Instant` is not serializable — put elapsed-since-start on the
   wire and reconstruct client-side (`started_at_utc: SystemTime` is already
   present as the anchor).
2. **Add a `view` command** to the inspector server returning that snapshot
   (plus a bounded log tail).
3. **Point `attach --tui` at the same renderer** — deserialize, then call the
   same `status_fold` + painter the console uses.
4. The console path **does not change**; it keeps loading in-process.

Identical views then hold *by construction* rather than by discipline: there is
one fold and one painter, and every screen is a transport in front of them.

### Decision: render client-side, not server-side

Ship the **snapshot**, not a rendered frame. A frame is baked to one terminal
width, so a second terminal of a different size gets a wrong-width view — and
the gutter layout is width-dependent throughout (the histogram's history/tail
split is chosen from the cell width; value cells right-align to the divider).
Client-side rendering lets a narrow SSH window and a wide local terminal each
lay out correctly from the same state.

Server-side frames are the cheaper build and would guarantee identical output,
so they are the fallback if snapshot serialization turns out to be worse than it
looks — but they cap the feature at one width.

## The multi-USER blocker (not a code problem)

Multiple **terminals of the same user** work as soon as the renderer is shared.
Multiple **users** do not, for a filesystem reason: the socket lives under
`XDG_RUNTIME_DIR`, which is `/run/user/<uid>` with mode `drwx------`. Another
user cannot traverse it regardless of the socket's own permissions.

Two options, and they are different products:

- **Shared socket path + group permissions.** Cheap, stays terminal-native.
  But note the endpoint has a *control* surface, not only reads (`set`,
  `controls`), so widening access has a security dimension the rest of this work
  does not. Needs an explicit auth/authorization decision, not just a chmod.
- **`nmbrs web`.** Already a daemon; the natural multi-user answer. One process
  reads the snapshot and serves many browsers, with no per-user filesystem
  permissions involved. If the web view rendered from the same snapshot through
  the shared fold, it becomes a third screen for nearly free once the refactor
  above lands.

## Recommended order

1. Shared-renderer refactor (items 1–4). Prerequisite for every other screen;
   unblocks multi-terminal immediately.
2. Then choose socket-permissions vs. web, based on whether "other users" means
   teammates on this box or people who would rather have a URL. The socket
   permission model should not be touched before that question is answered.
