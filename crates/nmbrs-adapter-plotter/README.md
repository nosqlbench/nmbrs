# nmbrs-adapter-plotter

The `plotter` adapter for [nmbrs](https://crates.io/crates/nmbrs). It collects
the value of every op field on every cycle. When the run ends, it draws them as
a plot in the terminal using Unicode braille characters. Use it to check that a
distribution, function or pattern produced by a workload's bindings looks the
way you expect.

## Using it

This adapter is used through the `nmbrs` CLI. Select it with `adapter=plotter`
(alias `adapter=plot`).

Each op field is one data series, named after the field. Integers, floats and
booleans (as 0 and 1) are recorded. Values of other types are ignored. Nothing
is sent anywhere. The plot is printed once, when the run shuts down. Running
this adapter turns the dashboard TUI off, because both need the terminal.

### Adapter parameters

| Param | Default | Effect |
|-------|---------|--------|
| `mode` | `auto` | `plot`, `histogram` (alias `hist`), `parametric` (alias `xy`), `polar`, or `auto`. |
| `lanes` | one lane per field | Groups fields into stacked lanes. `;` separates lanes and `,` separates fields within a lane. For example, `lanes=a,b;c` puts `a` and `b` in one lane and `c` in another. |
| `width` | terminal width minus 1 | Plot width in character cells. Falls back to 120 if the terminal size is unknown. |
| `height` | terminal height | Total height in rows. The plot area is 4 rows less. Falls back to 30 if the terminal size is unknown. |
| `no_color` | `false` | `true`, `1` or `on` turns off color. Color (24-bit) is used only when stdout is a terminal. |

Modes:

| Mode | Drawing |
|------|---------|
| `plot` | Each field's values in sample order, one lane per field (or per `lanes` group). |
| `histogram` | Each field's values binned across the width, with bar height showing how many values fell in each bin. |
| `parametric` | A scatter plot with the field named `x` on the X axis and `y` on the Y axis. If those names are missing, the first two fields are used. |
| `polar` | A radius field (`r`, `radius` or `rho`) and an angle field (`theta`, `angle` or `phi`), in radians, converted to x/y. If those names are missing, the first two fields are used. |

With `mode=auto`, the field names pick the mode: a radius name together with an
angle name gives `polar`, `x` together with `y` gives `parametric`, and anything
else gives `plot`.

### Examples

From [`visual/polar_rose.yaml`](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs/examples/workloads/visual/polar_rose.yaml).
`mode` is left out, and the field names `r` and `theta` select `polar`:

```yaml
params:
  adapter: plotter
  k: "3"
  cycles: "3000"

bindings: |
  input cycle: u64
  theta := to_f64(cycle) * 0.01
  r := cos(theta * k)

ops:
  point:
    r: "{r}"
    theta: "{theta}"
```

From [`visual/distribution/histogram.yaml`](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs/examples/workloads/visual/distribution/histogram.yaml):

```yaml
params:
  adapter: plotter
  cycles: "6000"
  mode: histogram

bindings: |
  input cycle: u64
  uniform     := unit_interval(hash(cycle))
  normal      := dist_normal(unit_interval(hash(hash(cycle))), 0.5, 0.15)
  exponential := dist_exponential(unit_interval(hash(hash(hash(cycle)))), 2.0)

ops:
  sample:
    uniform: "{uniform}"
    normal: "{normal}"
    exponential: "{exponential}"
```

```bash
nmbrs run workload=crates/nmbrs/examples/workloads/visual/polar_rose.yaml k=5 cycles=5000
```

More examples are in
[`crates/nmbrs/examples/workloads/visual/`](https://github.com/nosqlbench/nmbrs/tree/main/crates/nmbrs/examples/workloads/visual).

## Cargo features

None.

## Where it sits

- Implements `DriverAdapter` and `OpDispenser` from
  [nmbrs-runtime](https://crates.io/crates/nmbrs-runtime) and registers itself
  under the names `plotter` and `plot` via `inventory`.
- Reads op templates from
  [nmbrs-workload](https://crates.io/crates/nmbrs-workload). Terminal size and
  TTY detection use [crossterm](https://crates.io/crates/crossterm).
- For Rust callers, the crate exports `PlotterAdapter` and `PlotterConfig`.

## Links

- Repository: https://github.com/nosqlbench/nmbrs
- API docs: https://docs.rs/nmbrs-adapter-plotter
- nmbrs CLI: https://crates.io/crates/nmbrs

## License

Apache-2.0
