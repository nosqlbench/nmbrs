# Cursor Partitions — Operator & Workload-Author Guide

Partitioning lets you run a workload over a slice of its domain,
sweep it across many slices, or time-box each slice — all from
the command line, without editing the workload. This guide is
task-oriented; the full language reference is
[SRD 71](../sysref/71_cursor_partitions.md).

## The one-minute version

A workload opts in by declaring its cursors `over` a partition
source:

```yaml
phases:
  ann_query:
    bindings: |
      cursor q = range(0, query_count(prebuffered)) over cursor
```

From then on the operator controls the slice with the `cursor=`
parameter:

```
nmbrs run workload=my.yaml cursor=0..1%          # smoke: first 1%
nmbrs run workload=my.yaml cursor=50%..100%      # second half
```

Cursors without an `over` clause ignore the parameter entirely —
nothing is partitioned silently.

## Spec language quick reference

A spec follows `chunking [in window] [order]`:

| You want | Spell it |
|---|---|
| First 1% (smoke test) | `0..1%` |
| Rows 1000–4999 | `1000..5000` |
| First 90%, then 1% chunks until used up | `90%,1%,...` |
| First 90%, then the rest in 10 chunks | `90%,*/10` |
| First 90%, then exactly ten 1% chunks | `90%,1%x10` |
| First 90%, then Fibonacci-proportioned rest | `90%,*/fib:5` |
| Head and tail, skip the middle | `10%,~80%,10%` |
| Sixteen equal slices | `linear:16` (or `*/16`) |
| Exponential ramp | `mul:2` / `geom:8,2` |
| The middle half, in 5 chunks | `linear:5 in 25%..75%` |
| Biggest slice first | `fib:7 largest_first` |
| Slices in a (reproducible) shuffled order | `linear:16 random` |

Numbers carry their meaning in their shape: `53%` percentage,
`0.53` fraction, `1000` absolute ordinal. Order keywords:
`unchanged | smallest_first | largest_first | random` (size
sorts are named for their axis; `random` is seeded from the
spec, so the same spec shuffles the same way every run).

## Sweeping: iterate a multi-partition spec

A multi-partition spec needs an iteration — the scenario tree
walks the list and each phase run narrows to one partition:

```yaml
params:
  cursor: "2%,10%,*"

scenarios:
  sweep:
    - for: "p in cursor.partitions"
      phases: [ann_query]

phases:
  ann_query:
    bindings: |
      cursor q = range(0, query_count(prebuffered)) over p
```

Each iteration logs a banner: `partition 1/3 [0..20)`, … A
multi-partition spec on a cursor consuming it *directly* (no
`for:`) is a startup error — never a silent partition-0 run.

`partitions(spec[, extent])` is the inline form when you don't
want a param: `for: "p in partitions(\"linear:8\", 100000)"`.

## Reading the active partition

Inside a phase, the resolved partition is available three ways —
all the same values:

```yaml
bindings: |
  cursor q = range(0, N) over p
  i  := q.cursor.idx              # dotted scalar wires
  n  := q.cursor.partition_count
  lo := q.cursor.start_ordinal    # also: end_ordinal, start_pct, end_pct
  c  := cardinality(q.cursor)     # stdlib helpers on the Partition value
  qi := mod_in(cycle, q.cursor)   # per-cycle ordinal inside the partition
ops:
  emit:
    stmt: "slice {q.cursor.idx}/{q.cursor.partition_count} row={qi}"
```

The dotted spellings work in `{...}` text interpolation too.
Other helpers: `start_of`, `end_of`, `idx_of`, `count_of`,
`at(p, i)` (bounds-checked), `clamp_in(n, p)` (saturating),
`random_in(p, seed)` (deterministic hash).

## Time-boxing a slice: open-extent cursors

`until_*` cursors compose with partitions — the policy keeps its
time/pass/count semantics, and the partition end is a hard cap:

```yaml
bindings: |
  cursor q = until_elapsed(50, 1000) over p
```

A slice smaller than the budget finishes the moment it's
exhausted (no wrap-around, no idling); a slice bigger than the
budget stops on time. See
[`nmbrs/examples/workloads/cursors/timeboxed_partition_sweep.yaml`](../../nmbrs/examples/workloads/cursors/timeboxed_partition_sweep.yaml)
for a complete runnable demonstration.

One rule: an open-extent cursor has no extent to resolve a spec
*string* against, so `until_elapsed(...) over cursor` with
`cursor=0..10%` is an error — resolve the spec against a real
extent first (`for: "p in partitions(\"0..10%\", <extent>)"`)
and bind `over p`.

## Nested subdivision

Split a coarse sweep into a fine one without re-parsing specs:

```yaml
scenarios:
  hierarchical:
    - for: "outer in partitions(\"50%,*\", 1000)"
      phases:
        - for: "inner in subdivide(outer, 5)"
          phases: [walk]
```

`subdivide(p, n)` also works in bindings, returning a
`PartitionList`; its boundaries are identical to the `*/n` spec
token.

## Per-phase operator control

Scope a CLI override to specific phases by prefixing the param
with a phase pattern (bareword, glob, or regex):

```
nmbrs run workload=my.yaml ann_query.cursor=fib:7      # one phase
nmbrs run workload=my.yaml '*_query.cursor=0..10%'     # every *_query phase
```

Resolution order per phase: exact phase name beats glob; two
distinct globs matching the same phase for the same param is a
fatal ambiguity; otherwise the workload-wide `cursor=` applies,
then the workload's `params:` default. A pattern that matches no
phase is a startup error (it's a typo, not a preference).

Workloads can also *reify* their own knobs — declare
`warmup_cursor` / `steady_cursor` params and bind each phase's
cursor `over <param>`; operators then override each
independently without knowing phase names. The coverage workload
(`nmbrs/examples/workloads/cursors/cursor_partitions_coverage.yaml`)
demonstrates every shape in this guide, one scenario per shape.
