# Color & style guide

All readout and log output in nmbrs is colorized by default
when stderr is a TTY. This document is the canonical
reference for *what* colors mean and *where* they come from.

The authoritative specs are **SRD-46** (palettes / plots)
and **SRD-63 §5.2** (readout body color grammar).

---

## Color is on by default

The runtime decides whether to emit ANSI escapes by checking
[`crate::observer::use_color()`](../../crates/nmbrs-runtime/src/observer.rs)
once per process. It returns `true` when **both**:

- `stderr` is a TTY (interactive terminal, not a pipe / CI
  capture).
- The conventional `NO_COLOR` env var (https://no-color.org)
  is **unset**.

If either condition fails, color is off and readouts emit
plain text. Operators don't need to opt in; pipelines /
log archives stay clean because the TTY check excludes them.

Override knobs (in order of precedence):

1. `NO_COLOR=1` — disable everywhere, no matter what.
2. TTY presence on stderr — the default decision.

There's intentionally no `FORCE_COLOR`-style overrider. If
you need colored output captured to a file, use `script(1)`
or `unbuffer`.

---

## The default palette: Wong

nmbrs ships **Okabe-Ito 8-color** (also known as Wong, from
the Nature Methods 2011 editorial) as the default palette —
across plots (SRD-46) AND readout text. It's tested against
the three common forms of color-vision deficiency
(protanopia, deuteranopia, tritanopia) and stays readable
on both light and dark backgrounds.

The 8 entries are defined once in
[`crates/nmbrs/src/palette.rs::PALETTES`](../../crates/nmbrs/src/palette.rs):

| Name      | sRGB                | Used for             |
|-----------|---------------------|----------------------|
| black     | `(  0,  0,  0)`     | scale / axis lines   |
| orange    | `(230,159,  0)`     | series 1, WARN       |
| sky-blue  | `( 86,180,233)`     | series 2, INFO       |
| green     | `(  0,158,115)`     | series 3, OK         |
| yellow    | `(240,228, 66)`     | series 4             |
| blue      | `(  0,114,178)`     | series 5             |
| red       | `(213, 94,  0)`     | series 6, ERROR      |
| pink      | `(204,121,167)`     | series 7             |

Other palettes (`tol_bright`, `viridis_5`, `ibm`, etc.) live
in the same file. They're switchable per-plot via
`palette=<name>` on a `report:` block. Readouts currently
only use Wong; future revisions may add a runtime selector.

---

## Semantic style names

Inside readouts, **use semantic names, not raw colors**.
The mapping lives in
[`crates/nmbrs-runtime/src/readouts/color.rs::StyleName::resolve`](../../crates/nmbrs-runtime/src/readouts/color.rs).

| Style      | Meaning                              | Wong-derived ANSI      |
|------------|--------------------------------------|------------------------|
| `OK`       | Success, completed phase             | green `(122,193, 66)`  |
| `ERROR`    | Failure, error count > 0             | red `(214, 40, 40)`    |
| `WARN`     | Caution, non-fatal anomaly           | yellow `(247,201, 72)` |
| `INFO`     | Identity, names, neutral highlight   | sky `( 77,201,246)`    |
| `HEADER`   | Top-of-section emphasis              | white `(255,255,255)`  |
| `SUBHEAD`  | Secondary heading                    | grey `(180,180,180)`   |
| `EMPHASIS` | Operator-attention emphasis          | bright white           |
| `MUTED`    | De-emphasised tail (duration, hints) | dim (ANSI 2)           |

**Rule of thumb**: a readout author who needs to say
"this thing succeeded" writes `OK`, not `GREEN`. The
mapping survives palette swaps; raw color names don't.

The grammar accepts three shapes (per SRD-63 §5.2):

```
@OK phase_name        # semantic style — recommended
@GREEN phase_name     # direct color — escape hatch only
@#7AC166 phase_name   # hex — escape hatch only
```

---

## When to color what

The following table is the load-bearing convention. Add to
it when introducing a new readout; don't invent ad-hoc
colors at the call site.

| Surface                                | Color                |
|----------------------------------------|----------------------|
| `✓` glyph (phase done)                 | `OK`                 |
| `[ok]` bracket marker                  | `OK`                 |
| `[!!]` bracket marker, error counts    | `ERROR`              |
| `[..]` bracket marker (running)        | `INFO`               |
| `[  ]` bracket marker (not run)        | `MUTED`              |
| Phase / activity name                  | bold + `INFO`        |
| Scope-coords (`profile=…, k=…`)        | bold + `WARN` (yellow) |
| Counters with errors/retries > 0       | `WARN`               |
| Counters with errors/retries == 0      | `MUTED`              |
| Duration, ETA, hints                   | `MUTED`              |
| Section headers (`session:`, `phases:`)| `HEADER`             |
| Sub-totals, secondary labels           | `SUBHEAD`            |

Existing readouts that already follow this (good
references):
- `crates/nmbrs-runtime/src/readouts/builtins/phase_done.rs`
- `crates/nmbrs-runtime/src/readouts/builtins/phase_status.rs`
- `crates/nmbrs-runtime/src/readouts/builtins/scope_open.rs`

---

## Log-level coloring

The async log sink colorizes by severity (independent of the
readout color grammar). Mapping is in
[`colorize_log_line`](../../crates/nmbrs-runtime/src/observer.rs):

| Level | Style                              |
|-------|------------------------------------|
| TRC   | dim grey (`\x1b[2;90m`)            |
| DBG   | dim (`\x1b[2m`)                    |
| INF   | terminal default (no override)     |
| WRN   | yellow (`\x1b[33m`)                |
| ERR   | bold red (`\x1b[1;31m`)            |

INF intentionally has no override so user-themed terminals
show the operator's preferred default. Don't fight this.

---

## Anti-patterns

- **Inline raw escapes in production code** — use
  `ctx.use_color()` + the `ColorSpec`/`StyleName` helpers.
  Hard-coded `\x1b[32m` survives `NO_COLOR=1` only by
  accident.
- **Inventing new color names at the call site** — if you
  reach for a color that isn't in `StyleName`, add the
  variant + Wong mapping in one place, then use it.
- **Mixing direct colors with semantic styles in the same
  readout** — readers learn one convention; mixing is noise.
- **Coloring something already implied by structure** —
  e.g. don't paint the `:` in `key: value`. Color carries
  meaning; decorative use dilutes the channel.
