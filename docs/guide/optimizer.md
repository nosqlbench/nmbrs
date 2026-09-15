# Optimizer — Workload-Author Guide

An `optimize:` block turns a phase's `for_each` sweep into a **search**: instead
of visiting every coordinate, an optimizer proposes coordinates, reads an
**objective** you name, and maximizes it. This guide is task-oriented; the full
design is [SRD-86](../SRD/86_optimization.md). Every form below has a runnable
example under `nmbrs/examples/workloads/optimizer_*.yaml`.

> **Discover what's installed:** `nmbrs describe optimizers` lists the search
> methods, and `nmbrs describe controls` lists the live controls you can `servo:`
> — both read from *this* binary, so they're never out of date.

## The one-minute version

A phase becomes a search node by adding `optimize:` alongside its `for_each`:

```yaml
phases:
  sweep:
    cycles: 1
    for_each: "ef in 1.0,2.0,3.0,4.0,5.0"   # the search axis (auto-gathered)
    optimize:
      objective: score      # a wire to MAXIMIZE
      max_evals: 10
    bindings: |
      score := 0 - (ef - 3) * (ef - 3)      # peaked at ef=3
    ops:
      probe: { adapter: stdout, stmt: "ef={ef}" }
```

`nmbrs run workload=...` reports `best [3] → score=0`. Two things you do **not**
write: the axes (auto-gathered from `for_each`), and — with no `method:` — the
optimizer: it defaults to **`sweep`** (the identity: evaluate every coordinate,
report the best). Add `method: cmaes` (etc.) to search adaptively instead.

## Pick the axis kind by how you write the `for_each`

| Axis kind | `for_each` form | Notes | Example |
|---|---|---|---|
| **Discrete** | `"ef in 1.0,2.0,3.0"` or `"x in [1.0,2.0,3.0]"` | numeric detents. **Use float literals** (`1.0`, not `1`) — an integer axis types the iter-var `u64`, and signed objective math like `0 - (x-3)^2` then underflows. | `optimizer_sweep.yaml` |
| **Continuous** | `"ef in 1.0 .. 5.0"` | a real interval — *sampled* by a metric-space solver, not enumerated. | `optimizer_continuous.yaml` |
| **Categorical** | `'mode in ["a","b","c"]'` | named options, no order/distance. **Quote the labels** — a bare word is a wire reference (SRD-18f). | `optimizer_categorical.yaml` |

### One axis or several

A multi-clause `for_each` searches several axes jointly:

```yaml
for_each: "x in 0.0 .. 6.0, y in 0.0 .. 8.0"   # 2-D continuous search
optimize: { method: cmaes, objective: score, max_evals: 60 }
```

Discrete and continuous axes both work at any cardinality. See
`optimizer_multiaxis.yaml`.

## Choose a method

`nmbrs describe optimizers` lists them. The two classes:

- **Identity** — `sweep` (the **default**, used when `method:` is omitted) visits
  every coordinate in order and reports the best. Use it for small grids and
  categorical choices.
- **Adaptive** — `nelder_mead`, `cmaes`, `hooke_jeeves`, `bobyqa`, `bayes_opt`,
  `hyperband`, `centroid_variant`, `cost_greedy_traversal`. These propose
  coordinates from feedback and converge on a manifold without visiting
  everything. `nelder_mead` (simplex) and `cmaes` (population) are good defaults
  for continuous numeric tuning.

**Reproducibility — `seed:`.** Adaptive methods are *stochastic* — a random
starting simplex, population sampling, acquisition-function jitter — so two runs
of the same workload can converge to different coordinates. Set `seed:` in the
`optimize:` block to pin the RNG for a repeatable search:

```yaml
optimize: { method: cmaes, objective: score, max_evals: 60, seed: 42 }
```

This matters for regression tests (a flapping `best [...]` line is otherwise
expected) and for comparing two workload variants on equal footing. The `sweep`
identity method is deterministic and ignores `seed:`.

## Choose the objective

`objective:` is the value to **maximize**. Two sources:

- **Synthetic** — a pure function of the coordinate (deterministic, no backend).
  Good for learning and for tuning against a known manifold.
- **Run-produced** — a live metric the phase *generates*, read through a windowed
  reader (`metricsql_scalar("sum(rate(errors_total[3s]))")` or
  `metric_window(...)`). The value exists only *after* the phase runs, so the
  optimizer settles it across the run (the cadence-fed detector holds the
  windowed value until it stabilizes). See `optimizer_saturation.yaml` /
  `optimizer_metricsql.yaml`. Maximize, so negate a cost.

Either source can be written **two ways** — pick whichever reads cleaner:

```yaml
# Named wire — declare it in bindings:, then reference it.
bindings: |
  score := 0 - metricsql_scalar("sum(rate(errors_total[3s]))")
optimize: { objective: score }
```
```yaml
# Inline expression — write the polydat expression directly, no bindings: entry.
optimize: { objective: "0 - metricsql_scalar(\"sum(rate(errors_total[3s]))\")" }
```

A bare identifier (`objective: score`) is read directly; anything with operators
or calls is lowered to a synthesized `__objective` wire. See
`optimizer_inline_objective.yaml`.

When the objective is **all** you're setting (default `method: sweep`, no
`servo:`), drop the map entirely — a bare string `optimize:` value *is* the
objective:

```yaml
optimize: |
  0 - metricsql_scalar("sum(rate(errors_total[3s]))")
```

This is exactly `optimize: { objective: "<that string>" }`. Add the map back the
moment you need `method:`, `max_evals:`, `servo:`, etc.

### What "settle" means (run-produced objectives)

A run-produced objective has no value until the phase has run for a bit, and
it's *noisy* — it drifts while load ramps and the metric window fills. So the
optimizer doesn't read it once; it **settles** it: a cadence-fed detector
samples the windowed value every metrics tick and holds it until it *stabilizes*
(stops trending), then records that as the coordinate's score. If the phase ends
before it settles, the last smoothed value is used. Two rules follow:

- **Use a *windowed* reader, not a session-cumulative one.**
  `metricsql_scalar("sum(rate(errors_total[3s]))")` and `metric_window(...)`
  report a *per-window* value, so the previous setting's contribution clears out
  of the window. A bare `metric(...)` (a session-lifetime total) can't be
  isolated to one setting — the optimizer warns, and the number is meaningless
  across coordinates. This is the windowed-objective rule a `servo:` axis
  enforces.
- **The window is the warmup.** Before a setting's score is trusted, the
  detector waits at least one window length (auto-sized to the objective's
  widest rollup) so the window has fully cleared the *prior* setting. That dwell
  is why a servoed control pauses at each setting before stepping — and why a
  control phase needs enough `cycles:` to outlast the search.

## Coordinate vs control: stepping through vs servoing

How a value is enacted has two modes:

- **Coordinate** (the **default** for every axis) — the value is **stepped
  through by re-running the phase** at each setting.
- **Control** — a live SRD-23 control (`concurrency`) **retargeted without
  restarting the phase**, servoed by a daemon on one continuous phase.

You get control by **naming it in `servo:`**. Two equivalent ways to name it:

```yaml
# Direct — the axis IS the control (no bind wire). The only way to servo `rate`.
for_each: "concurrency in 32, 16, 2"
optimize: { objective: score, servo: concurrency }
```
```yaml
# Indirect — a plain var bound into a control, when you want a different axis name.
concurrency: "{conc}"
for_each: "conc in 32, 16, 2"
optimize: { objective: score, servo: conc }
```

`servo:` accepts one name (`servo: concurrency`) or a list (`servo: [concurrency,
rate]`). The optimizer servos the control per setting and settles the windowed
objective at each. Because the phase dwells at each setting, a control sweep that
explores a saturating region must **not** carry a tripping error guard — set
`error_rate_max: 1.0` (which compiles to `error_rate > 1.0`, i.e. "allow 100%").
See `optimizer_control.yaml`.

**`servo:` is validated, not guessed.** A servoed var must (a) be a search axis,
(b) resolve to a live control — directly (its name is `concurrency`/`rate`) or via
a `{var}` bind — and (c) have a windowed objective the servo can settle. Miss
any of these and you get a **clear error** — never a silent downgrade. So if you
wire a control but use a session-cumulative
`metric(...)` objective (which can't isolate a live setting), don't `servo:` it —
just step it through (the default), as `optimizer_saturation.yaml` does.

> Which names are live controls? Run **`nmbrs describe controls`** — it lists every
> control this binary can servo (core `concurrency`/`rate` plus any adapter knobs
> like `cql_trace_rate`), with the condition under which each appears (SRD-23).

### Servoing several controls at once

Name more than one axis to servo a multi-dimensional control space on **one**
continuous phase — the daemon retargets *every* named control at each setting:

```yaml
for_each: "concurrency in 32, 2, rate in 4000, 1000"
optimize:
  objective: score
  servo: [concurrency, rate]        # both retargeted together, per grid point
```

A `servo:` list mixes resolution forms freely: `servo: [conc, rate]` with
`concurrency: "{conc}"` servos the concurrency control *indirectly* (via the
`{conc}` bind) and `rate` *directly*, in the same search. See
`optimizer_multiservo.yaml`. (This is distinct from a *hybrid* below: there every
listed axis is servoed; a hybrid leaves some axes to step through.)

### Mixing step-through and servoing in one node (hybrid)

A single node can do both — name only the servoed vars:

```yaml
for_each: "batch in [1,2], conc in [16,2]"
optimize:
  objective: score
  max_evals: 10
  servo: conc                       # batch steps through; conc servos
```

`batch` (not servoed) forms the **outer re-run grid**; for each `batch` cell the
phase reruns and the daemon servos `conc` **interior** to it. A coordinate axis is
realized *only* by iterating its scope (re-run) — never set ineffectually. See
`optimizer_hybrid.yaml`.

## How a search renders

A search node is a **Search**, not a sweep: dryrun/describe and the scene tree
show its spec and budget (`search · cmaes · maximize score · {x, y} · ≤60 evals`)
and *evaluations* as children — never a pre-listed coordinate set. The final line
reports `best [...] → objective=value after N evals`.

## Common pitfalls

| Symptom | Cause | Fix |
|---|---|---|
| Objective underflows / garbage at the optimum | An integer axis (`x in 1,2,3`) types the iter-var `u64`, so `0 - (x-3)^2` wraps. | Use float literals: `x in 1.0,2.0,3.0`. |
| A categorical label resolves to a wire (or errors) | A bare word in a `for_each` list is a wire reference (SRD-18f). | Quote the labels: `mode in ["a","b","c"]`. |
| `optimize servo: X … not wired to a live control` | `X` isn't a control and no `{X}`-bind feeds one. | Name a control directly (`servo: concurrency`/`rate`), or wire it (`concurrency: "{X}"`); or drop it from `servo:` to step through. |
| `… needs a windowed objective … but the phase reads no live metric` | A servoed control needs a metric the daemon can settle per setting. | Read a windowed metric (`metric_window(...)` / `metricsql_scalar(rate(...[W]))`); or don't `servo:` it. |
| `servo: rate` rejected — no rate control | The `rate` control exists only when the phase sets `rate:`. | Add a `rate:` field (its value is the servo warmup). |
| A servoed phase ends in failure at a saturating setting | A tripping error guard fired while the daemon dwelled there. | Set `error_rate_max: 1.0` ("allow 100%") — a control phase deliberately dwells at saturation. |

## Example index

| Example | Form it illustrates |
|---|---|
| *Axes* | |
| `optimizer_sweep.yaml` | discrete axis, default `sweep` (identity) + best-selection, synthetic |
| `optimizer_continuous.yaml` | continuous axis, `nelder_mead`, synthetic |
| `optimizer_multiaxis.yaml` | multi-axis continuous, `cmaes` |
| `optimizer_categorical.yaml` | categorical (label) axis |
| *Objective* | |
| `optimizer_inline_objective.yaml` | objective as an inline expression (no `bindings:` entry) |
| `optimizer_saturation.yaml` | run-produced objective (metric reader), settled |
| `optimizer_metricsql.yaml` | run-produced objective (MetricsQL reader), settled |
| *Control / servo* | |
| `optimizer_control.yaml` | one control axis — live-retarget servoing daemon |
| `optimizer_multiservo.yaml` | two controls servoed together (`servo: [concurrency, rate]`) |
| `optimizer_hybrid.yaml` | mixed coordinate + control axes in one node |
