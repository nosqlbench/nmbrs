# Polydat pure-dataflow assumptions, foldability, and the `volatile` escape hatch

This guide explains the dataflow-purity model Polydat assumes about
node functions, how the compiler turns "purity" into
identity-affecting facts (const folding + `hash_const` for resume
identity), and the one trapdoor case the structural analysis
can't catch on its own — the case `volatile` exists for.

Audience: workload authors who need to reason about resume /
identity behavior, plus node-function providers extending GK
with new functions.

---

## What Polydat assumes about data flow

Every Polydat function is **a pure function of its wire inputs and
const args**. Same inputs in → same output out, no side effects,
no hidden state. That's the contract every node-function provider
signs when they `register_nodes!`.

The compiler relies on this in several places:

- **Const folding.** A node whose inputs are all const (literal
  values or upstream-folded const slots) can be evaluated once
  and replaced with a `Literal(value)` node. The folded program
  is what `hash_const` (the resume-identity hash) actually
  hashes — a workload param edit that flows through to a const
  slot changes the hash, which invalidates resume identity.

- **Validation.** A node with no wire inputs (and no `is_init`
  marker) is structurally classified as **non-deterministic**.
  Strict-mode compilation rejects these unless the author
  acknowledges the non-determinism (see `volatile` below). The
  built-in `current_epoch_millis()`, `counter()`, `thread_id()`
  trip this naturally; their help text explicitly says
  "NON-DETERMINISTIC."

- **Optimization.** Pure functions can be reordered, batched,
  cached, JIT-lowered to native code. The optimizer assumes the
  contract holds.

The mental model: the compiled program graph is a *value-flow
diagram* where every node is a math function, the inputs come
from cycle / capture / cursor / parent-scope wires, and the
outputs are deterministic transforms of those inputs.

---

## Normative cases

For most workloads, the model just works. Examples:

```
input cycle: u64
id      := mod(hash(cycle), 1000000)         # pure: cycle → u64
shard   := id % 16                           # pure: u64 → u64
name    := number_to_words(id)               # pure: u64 → String
greeting := "hello {name}"                   # pure: String → String
```

Every node is pure. Const folding works freely; identity is
deterministic; resume behavior is predictable.

Workload params and `env_or(...)` work too — they're
session-init substitutions that land their evaluated values in
const slots before `hash_const` runs:

```
params:
  dataset: example
bindings: |
  ds := "{dataset}"               # → final ds := "example"
  cap := env_or("CAP", "100")     # captured at session start
```

A workload that runs once with `CAP=100` then once with
`CAP=200` gets *different* `hash_const` values for any phase
that consumes `cap`. The resume planner correctly invalidates
the cached completion — the operator's env edit is part of
identity.

---

## The trapdoor case — pure-looking impurity

Polydat's structural detector classifies non-determinism by
**absence of wire inputs**. That's a heuristic, not a proof. A
node-function provider can write a function that *looks* pure
to the type system (takes const-string args, no wire inputs
that are obviously cycle-derived) but actually has hidden
state.

The canonical example:

```
data := read_file("/tmp/cache.json")
```

`read_file` takes a const-string path. By the structural rule,
it has empty wiring → flagged non-deterministic in strict mode.
That's correct! Two runs with the same `/tmp/cache.json`
content produce the same value; two runs with different content
don't. The detector caught it.

But consider a smarter `csv_field`:

```
val := csv_field(cycle, "/tmp/users.csv", "username")
```

This *does* take a wire input (`cycle`). The structural rule
says "has wires → deterministic." But the file content
(`/tmp/users.csv`) can mutate between runs. Two runs with the
same workload YAML and the same `cycle=42` could produce
different `val`. The detector misses it.

This is the trapdoor: a function that looks pure but reads
state from somewhere outside the wire graph. The compiler
can't validate node implementations against their declared
shape — workload authors and node providers share the burden
of keeping the contract honest.

**`hash_const` is most affected**: it folds const-foldable
nodes by evaluating them. If a "pure-looking impure" node gets
folded, its session-time value lands in the hash. Two runs
with mutated upstream state then produce *different* hashes
for the same workload — surprise IdentityMismatch. Or worse,
the value is captured early and never re-checked, leading to
stale identity.

---

## When and why to use `volatile`

`volatile` is the author-level escape hatch for the trapdoor
case. Mark a binding's wire `volatile` and the compiler:

1. **Excludes the wire's value from `hash_const`** — even when
   the source function would otherwise be foldable. The wire's
   AST shape (function name, const args) still hashes normally,
   so changing the *workload source* still invalidates
   identity; only the *evaluated value* is opaque.

2. **Suppresses the strict-mode "non-deterministic node"
   warning** for the immediate source node when it has empty
   wiring — `volatile` IS the author's acknowledgment.

3. **Propagates the non-fold behaviour downstream** via the
   existing det/non-det analyzer walk: any wire that reads from
   a `volatile` wire inherits the non-foldability.

Example of legitimate use:

```
# Reads /tmp/cache.json; content can mutate between sessions.
# The compiler can't know that, so the author acknowledges it
# explicitly and tells the resume planner to ignore the value.
volatile cache := read_file("/tmp/cache.json")
volatile derived := transform(cache)   # inherits volatility
```

A workload like this resumes correctly even if the cache file
has been edited between sessions — the cache content doesn't
contribute to identity, so the resume planner doesn't see a
spurious mismatch and the completed phases skip cleanly.

The test fixture at `nmbrs/examples/workloads/diagnostics/resume_test.yaml` is
the worked example: `testkit_side_effect_sequence_next_cycling` advances
a state file once per session, returning a different threshold
on each invocation. Without `volatile`, identity would
invalidate on every resume; with it, identity is stable across
the staircase test.

---

## When NOT to use `volatile`

`volatile` is for things that are **intentionally invisible to
identity**. If a value's variation *should* affect identity,
don't mark it volatile — let it fold. Examples of bad uses:

- An env-var that picks a real dataset name. If `DATASET`
  changes, identity should change too — that's the env hole
  the resume planner relies on. Use plain `env_or(...)` (which
  folds session-init values into const slots), not
  `volatile env_or(...)`.

- A workload param whose value tunes the cycles count. Different
  cycle counts mean different work; identity must distinguish.
  Plain workload params already self-fold.

- A debug counter you "don't care about." If you don't care,
  why are you reading it? Either commit to caring (so it
  affects identity) or write past it (so it never enters a
  binding the workload uses). `volatile` is for *committed*
  invisibility, not handwave-away.

Rule of thumb: would you want resume to skip a phase whose
inputs *changed* in this dimension? If yes → don't mark
volatile. If no → mark volatile.

---

## The trust contract

GK is a kernel-style API. Two parties share responsibility for
keeping the model honest:

**Node-function providers** (anyone who calls `register_nodes!`):

- Pure functions are the default contract. Don't read external
  state from inside a function that has wire inputs.
- If your function does have side effects or external reads,
  declare it with **empty wiring** so the structural detector
  catches it. The runtime then warns / rejects under strict
  mode unless a caller explicitly opts in.
- If a function reads session-static state (env, file content
  captured at session-init), make sure the value is captured
  in `new()` and the eval just emits the captured value. That
  matches how `env`, `env_or`, `tmp_dir`, `session_start_millis`
  all work today — they're "non-deterministic" structurally
  but session-stable in practice.

**Workload authors** (anyone writing Polydat source consumed by a
phase):

- The structural detector handles obvious cases. For
  pure-looking impurity (the trapdoor), reach for `volatile`.
- When in doubt about whether a third-party node has the
  trapdoor, look at its source. If it takes wire inputs but
  reads from `std::fs::*`, `std::env::*`, or a global cache,
  mark consuming wires `volatile`.
- The compiler can't enforce the contract from inside a node;
  it can only catch the structural cases. Misclassified nodes
  produce subtly-wrong identity behavior that the resume
  planner trusts. Be careful with third-party nodes.

---

## See also

- SRD-10 §"Binding Modifiers" — grammar reference for the
  modifier set.
- SRD-15 §"Const Constraint Metadata" — how const-arg constraints
  interact with the analyzer.
- SRD-44 §"Authors opt out of folding via `volatile`" — the
  resume-identity contract `volatile` is the escape hatch for.
- `docs/SRD/history/resumable_test_fixture.md` — the design memo
  that motivated the keyword, with the staircase fixture as
  worked example.
