# Polydat / SRD Audit and Reduction Plan

> **STATUS: EXECUTED (historical record).** This plan has been carried out — the
> polydat-internal SRDs are now redirect stubs or reduced nmbrs-side integration docs,
> and the substrate design lives in `polydat/docs/` (clean, axiom-delegating; no
> `imported/` backlog remains). The live front door is
> [SRD 09 Polydat Contract](SRD/09_polydat_contract.md). This file is retained as the
> record of *how* the consolidation was done and will be moved under `SRD/history/`
> in the archive pass (Part 4).

**Purpose.** Now that polydat owns the definitive design for
the variates + Polydat substrate (via five design docs in
`polydat/docs/design/`), the nmbrs-level SRDs in
`docs/SRD/` contain substantial duplication. This doc
audits each affected SRD against the polydat treatment and
proposes a reduction plan: what to delete, what to reduce,
what to keep, and where to add cross-references.

The audit also identifies **gaps** — places where polydat
implements something the design docs don't yet cover, and
places where SRDs make claims that polydat doesn't yet
implement.

## Pre-flight coverage review

Before any SRD reduction, a section-by-section coverage
review compared each proposed-DELETE/REDUCE SRD section
against the corresponding polydat doc content. The
review found a **substantive misframing** in the original
audit's reduction recommendations:

> **Polydat docs and SRDs operate at different abstraction
> tiers**, not as duplicates of the same content. Polydat
> docs (composition_substrate, grammar, graph_compiler,
> runtime_model, expression_engine) operate at the
> **axiom level** — they name invariants, state contracts,
> and identify load-bearing properties. The SRDs operate
> at the **mechanism level** — they contain concrete
> tables, examples, API surfaces, diagnostic formats, and
> "what works / what doesn't" enumerations that the
> polydat docs delegate via cross-reference.

This means the original audit's aggressive DELETE
recommendations would have stripped load-bearing detail
from the SRDs without porting that detail into polydat.
The revised disposition for most SRD sections is REDUCE
(keep mechanism detail, add cross-reference to polydat's
axiom-level framing) rather than DELETE.

### Concrete examples of the layered relationship

| SRD section | Polydat coverage | Detail polydat does NOT have |
|---|---|---|
| SRD-10 §Infix Operators (precedence table, operator→node mapping) | grammar.md §3.4 T-BinOp-Add (one example rule) | Full precedence table; complete operator catalog; comparison output types |
| SRD-10 §if(...) intrinsic | grammar.md §2.5 (only `call_expr` production) | `if` as compiler intrinsic; select_u64/f64 desugar; both-branches-evaluated semantic |
| SRD-10 §Literal Promotion | grammar.md §3.1 (T-IntLit/T-FloatLit type rules) | "Literal in wire position → const node" promotion mechanism |
| SRD-10 §String Interpolation | grammar.md §2.5 (one-line production) | Printf desugar; `{{ }}` escapes; format-spec interaction |
| SRD-10 §Compiler Diagnostics | (not covered) | Tag levels (info/advisory/warning); `--diagnose` flag |
| SRD-10 §Auto-Conversion to String | composition_substrate.md T2 (axiom); graph_compiler.md NF (mechanism) | From-type → to-type → adapter-name table |
| SRD-11 §Const Binding Contract Plan A/B | composition_substrate.md L2 (axiom) | Plan A wire-chain check; Plan B scope-init pull; specific diagnostic strings |
| SRD-11 §Two Evaluation Lifecycles classification table | composition_substrate.md L2 (axiom) | Per-producer table (Literal / iter-extern / capture / etc.) with effectively-const verdicts |
| SRD-13c §No Flattening / §How It Works | composition_substrate.md L1 / graph_compiler.md CF1-4 (axioms) | Concrete steps (output manifest extraction, bind_outer_scope copies values) |
| SRD-14 §Const Expression Evaluation §"What works / What does NOT work" | expression_engine.md §3.1 (axiom) | Concrete examples and exclusion list |

This pattern recurs across every SRD audited. The polydat
docs are deliberately abstract; the SRDs are deliberately
detailed. Both layers are load-bearing.

### Dissonances found and resolved

Two concrete dissonances were found between SRD text and
current code; both were fixed before the audit's reduction
plan was revised:

**Dissonance 1: SRD-10 §Node Contract — outdated `PolydatNode`
trait surface.** The SRD showed `fn evaluate(&self, ...)`
with only `meta` + `evaluate`; the real trait is
`fn eval(&self, ...)` plus `commutativity`,
`accepts_none_inputs`, `compiled_u64`, `jit_constants`,
and `purity` (γ-2). **Resolution:** updated SRD-10 to
match the real trait surface, with cross-reference to
composition_substrate.md §2 for the slot contract.

**Dissonance 2: SRD-14 §Const Expression Evaluation §API —
outdated `eval_const_expr` signature.** The SRD showed
`Result<Value, String>`; γ-3 migrated this to
`Result<Value, EmbeddingError>`. **Resolution:** updated
SRD-14 to show the typed signature with cross-reference
to expression_engine.md §6 for the error ontology.

### Revised reduction strategy

Given the layered-doc finding, the original audit's
recommendations need recalibration:

- **DELETE** is appropriate ONLY for SRD sections that are
  literal duplicates of polydat axiom statements — sections
  that say "the runtime memoizes per-generation" where
  polydat says the same thing. These are rare; most SRD
  sections marked DELETE in the original audit actually
  contain mechanism detail polydat does not cover.
- **REDUCE** with explicit "keep mechanism detail" is
  appropriate for sections that have BOTH axiomatic content
  (now in polydat) AND mechanism detail (still load-bearing
  in SRD). The reduction strips the axiomatic prose and
  inserts a cross-reference to polydat for the axiom.
- **CROSS-REF** without structural change is appropriate
  for sections whose content is mostly nmbrs-side framing
  but invokes polydat axioms in passing.
- **PORT** (new disposition) is appropriate for mechanism
  detail the SRD has but that genuinely belongs in
  polydat — e.g., the operator precedence table is more
  fundamentally a grammar concern than an nmbrs concern;
  porting it into grammar.md tightens the substrate spec.

### Revised reduction estimates

The original audit estimated ~28% reduction (~2,100 lines).
The revised estimate is closer to **~10–15% reduction
(~800–1,200 lines)** — most "DELETE 450 lines from SRD-10
§DSL Syntax" recommendations become "REDUCE: strip the
axiomatic prose (~60 lines), keep the mechanism detail
(~390 lines), add cross-reference to grammar.md (~5 lines)."

### What this means for the execution plan

1. **Land the two dissonance fixes first** — already done
   above.
2. **Re-do the per-SRD reduction tables** with the layered
   relationship in mind. Replace most DELETEs with
   REDUCE-with-detail-preserved. Identify the small
   subset of true DELETEs (literal axiom duplicates).
3. **For each PORT candidate**, decide: does the detail
   belong in polydat (move it) or stay in nmbrs (keep it
   in SRD with a cross-reference)? This is the only place
   the polydat docs grow as a result of the reduction
   plan.
4. **Execute the reduction passes** (originally Pushes
   A1-A6); the smaller per-push scope makes them faster
   to land but the overall structural reorg is less
   dramatic.

The remainder of this document (the §"Per-SRD reduction
recommendations" tables, the §"Execution plan", the
§"Gap analysis") is the **original audit** and should be
read as the unreduced version. A revision incorporating
the pre-flight findings — flagging which DELETEs should
become REDUCEs with detail preserved — is the next step.

---

## Pivot: Import-First Reorganization

After the pre-flight review surfaced the layered-doc
relationship (axioms vs mechanism), an attempt to revise
each SRD in place by interleaving "REDUCE / KEEP / PORT"
edits ran into a noise-vs-signal problem: too much of the
work is bookkeeping about cross-references that depend on
where the content ends up living, not on what's correct.

**New approach:** structurally move polydat-owned SRDs
into `polydat/docs/imported/` **first**, intact and
unrevised. Then, with the polydat-owned content
co-located inside the polydat crate, reconcile within
polydat (collapse duplication against the axiom-level
design docs, hoist mechanism detail upward, retire
overlapping content) without the noise of the outer
nmbrs SRD layout.

This separates two concerns:

1. **What is polydat's design surface?** — determined by
   the move list below. This is the structural decision.
2. **How should polydat's design surface be organized?**
   — deferred to a second pass that operates entirely
   inside `polydat/docs/`.

For nmbrs-side SRDs, the move replaces each migrated SRD
with a short stub pointing at the new location in
polydat. nmbrs-specific framing (workload integration,
activity invocation, op-template scope) that's currently
entangled with the polydat content stays in the SRD stub
or moves to a new nmbrs-side integration doc — handled
case-by-case in the per-SRD plan below.

### Move classification

Each candidate SRD falls into one of three tiers:

- **Tier 1 — Move wholesale.** SRD content is essentially
  polydat-internal; no nmbrs-specific framing to preserve.
  Replace nmbrs SRD with a one-line stub pointing into
  `polydat/docs/imported/`.
- **Tier 2 — Move wholesale, preserve nmbrs intro.** SRD
  content is polydat-owned but the SRD has an nmbrs-side
  framing intro (why nmbrs cares, how activity invokes
  the substrate). Move the whole doc; leave the SRD as
  a short stub with the nmbrs intro + pointer.
- **Tier 3 — Split before move.** SRD has substantive
  content on both sides — polydat substrate AND nmbrs
  workload/activity integration. Split into a polydat
  doc (moved) + an nmbrs-side integration SRD (stays).

### Per-SRD move plan

| SRD | Lines | Tier | Move target | nmbrs-side residue |
|---|---|---|---|---|
| SRD-10 Polydat Language and Compilation | 897 | 3 (split) | `imported/polydat_language.md` (DSL syntax, type system, node contract, wiring model, compilation pipeline) | `docs/SRD/10_polydat_language.md` keeps: "GK as unified access surface", "Reification: runtime state → Polydat wire", "Output Selection", "GK as Unified State Holder", "Op-Level Bindings", "Cursor Declarations" |
| SRD-11 Polydat Evaluation Model | 498 | 3 (split) | `imported/polydat_evaluation_model.md` (Program/State Split, Provenance Invalidation, Two Lifecycles, Const Binding, Input Spaces, Compilation Levels) | `docs/SRD/11_polydat_evaluation.md` keeps: FiberBuilder, Cursor-Driven Evaluation |
| SRD-12 Polydat Standard Library | 359 | 2 | `imported/polydat_stdlib.md` (whole catalog) | stub pointer |
| SRD-13 Polydat Modules | 109 | 3 (split) | `imported/polydat_modules.md` (module system semantics) | `docs/SRD/13_polydat_modules.md` keeps: Compiler Diagnostic Event Stream |
| SRD-13b Polydat Combination Modes | 208 | KEEP nmbrs | — | nmbrs-side terminology doc; add cross-refs to polydat |
| SRD-13c Polydat Scope Model | 891 | 2 | `imported/scope_model.md` (whole) | stub with What-This-Does-NOT-Change + Open Design Issue + Design Rationale |
| SRD-13d Op-template Polydat Scope Layer | 779 | KEEP nmbrs | — | nmbrs-runtime-specific; add cross-refs |
| SRD-13e Scope-as-Module Refinement | 678 | KEEP nmbrs | — | nmbrs-side typed ScopeModule on top of polydat substrate; add cross-refs |
| SRD-13f Cross-Scope Wire Materialization | 944 | 3 (split) | `imported/wire_materialization.md` (architectural model, materialization gradient, cell classification) | `docs/SRD/13f_*.md` keeps: synthesizer rule, plan-to-true-up, open questions |
| SRD-14 Polydat Config Expressions | 170 | 3 (split) | `imported/config_expressions.md` (Expression Syntax, Const Expression Evaluation, embedding mechanics) | `docs/SRD/14_*.md` keeps: Resolution Order, Param Substitution Interaction (host-side param resolution) |
| SRD-15 Strict Mode | 697 | KEEP nmbrs | — | nmbrs-side strict-mode policy; add cross-ref to expression_engine.md §5.1.3 |
| SRD-16 Polydat Engines | 225 | 2 | `imported/polydat_engines.md` (Compilation Levels, Provenance Optimization, Engine Selection, Type System) | stub pointer |
| SRD-16b Polydat JIT Wiring | 351 | 1 | `imported/jit_wiring.md` (whole) | stub pointer |
| SRD-66 Runtime Feature Detection | 1217 | 3 (split) | `imported/runtime_features.md` (polydat stdlib nodes: pick / exactly_one_value / log_*; kernel-driven polydat-call slot contract) | `docs/SRD/66_*.md` keeps: vari-structured `result:` field shape, OpResult body projection, workload migration story |
| SRD-67 Parent-gated Subcontext Construction | 1030 | 2 | `imported/subcontext_construction.md` (whole walled-off protocol) | stub with nmbrs-side composition-with-13e cross-ref |
| SRD-74 None Propagation | 302 | 2 | `imported/none_propagation.md` (three orthogonal rules) | stub with workload-author behavior + test contract pointers |

### Out of scope for the move

These SRDs are polydat-adjacent but their content is
host-side; they stay in `docs/SRD/`:

- SRD-13b (Combination Modes terminology), SRD-13d
  (op-template scope layer), SRD-13e (typed ScopeModule
  for nmbrs), SRD-15 (strict-mode host policy)
- All workload-level SRDs (20-22, 30-44, 60-66 except 66,
  71-77)
- SRD-02 (Concurrency Model) — host-side
- Comprehension SRDs (18b/c/d/e, 78) — already
  cross-referenced to polydat's `comprehension_forms.md`

### Tier-3 split policy

For Tier-3 SRDs, the split happens at the same time as
the move:

1. Identify the polydat-internal sections (axioms,
   contracts, syntax, semantics that belong to the
   substrate regardless of host) and the nmbrs-internal
   sections (workload framing, activity invocation,
   op-template-specific behavior).
2. Create the `polydat/docs/imported/<name>.md` with
   the polydat-internal sections, header preserved.
3. Leave the nmbrs-side SRD with the nmbrs-internal
   sections + a header pointing at the moved companion.
4. Add a manifest note at the top of the imported doc:
   "Imported from `docs/SRD/<N>_<name>.md` — pending
   reconciliation against polydat axiom docs."

### Pending-reconciliation manifest

Each imported doc carries a top-of-file YAML-ish manifest:

```markdown
---
imported_from: docs/SRD/10_polydat_language.md
imported_on: 2026-05-30
reconciliation_status: pending
overlaps_with:
  - polydat/docs/design/grammar.md (§3 productions, §3 type rules)
  - polydat/docs/design/composition_substrate.md (§2 slot contract)
  - polydat/docs/design/graph_compiler.md (§2 pipeline)
---
```

The `overlaps_with` list seeds the reconciliation pass:
each entry names where the content competes with an
existing axiom-level treatment. The reconciliation pass
visits each manifest, decides per-overlap which doc owns
the content (collapse into axiom, keep both with explicit
delegation, hoist mechanism up into axiom doc), and
drains the imported docs over time.

### Execution sequence

1. **Push I-0 — scaffolding.** Create
   `polydat/docs/imported/` with a `README.md` explaining
   the import-first workflow and the reconciliation
   process.
2. **Push I-1 — Tier-1 moves.** SRD-16b, SRD-66. Smallest
   scope; validates the move pattern + stub convention.
3. **Push I-2 — Tier-2 moves.** SRD-12, SRD-13c, SRD-16,
   SRD-67, SRD-74. Move-with-stub for SRDs that are mostly
   polydat-owned with thin nmbrs framing.
4. **Push I-3 — Tier-3 splits.** SRD-10, SRD-11, SRD-13,
   SRD-13f, SRD-14. Each requires a per-section split
   decision before moving the polydat-internal part.
5. **Cross-reference updates.** Every nmbrs SRD that
   pointed at a moved SRD section needs its link updated
   to point at the new `polydat/docs/imported/` location.
6. **Reconciliation pass(es).** Deferred — handled inside
   `polydat/docs/` only, draining the imported docs by
   reconciling each `overlaps_with` entry against the
   axiom-level design docs.

### What this approach buys

- **Atomic visibility into polydat's design surface.**
  After Pushes I-1/I-2/I-3, every polydat-owned design
  doc lives under `polydat/docs/`. No external dependence
  on `docs/SRD/` for substrate concerns.
- **Reconciliation noise stays inside polydat.** The
  axiom-vs-mechanism collapse decisions happen against a
  fixed input set (the imported docs) instead of being
  intertwined with nmbrs SRD edits.
- **Reversible.** Each move is a structural file rename
  with a manifest header; no content is lost, and the
  reconciliation can take as long as it needs without
  blocking other work.
- **Independent decision tracks.** "Should SRD-10 §DSL
  Syntax be deleted in favor of grammar.md §3?" becomes
  a question answerable inside polydat, after the move,
  with both texts side by side.

---

## Methodology

For each SRD, sections are categorized:

| Disposition | Meaning |
|---|---|
| **DELETE** | Fully owned by polydat now; remove the section, add a one-line pointer. |
| **REDUCE** | Overlap with polydat but has nmbrs-specific scope; trim to the integration concerns; cross-reference polydat for the substrate. |
| **KEEP** | Genuinely nmbrs-side (workload integration, host concerns); no polydat overlap. |
| **REWRITE** | Section is correct in spirit but framed before polydat existed; reframe as nmbrs-integration with polydat as the substrate. |
| **CROSS-REF** | Add or improve a cross-reference; no structural change. |

The polydat docs that own definitive content:

- [composition_substrate.md](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md) — S/T/L axioms; slot contract; the three pillars (Context Synthesis, Type Safety, State Layering).
- [grammar.md](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/grammar.md) — G axioms; formal grammar productions; type-inference rules.
- [graph_compiler.md](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/graph_compiler.md) — H/CF/NF axioms; hoisting + Graph Fusion (Context Fusion + Node Fusion) pipeline.
- [runtime_model.md](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/runtime_model.md) — R/D axioms; data flow, caching, invalidation, determinism guarantees.
- [expression_engine.md](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/expression_engine.md) — E axioms; host-embeddable evaluation surface; embedding system contract.

Plus the existing comprehension treatment:
[comprehension_forms.md](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/comprehension_forms.md)
and related plan/cutover docs.

---

## Per-SRD reduction recommendations

### SRD-10: Polydat Language and Compilation (897 lines)

This SRD is the largest single source of overlap. The DSL
syntax, type system, wiring model, and pipeline overview
are now all formally specified in polydat docs.

| Section | Disposition | Action |
|---|---|---|
| §"GK as the unified access surface" | KEEP | nmbrs-side framing — why Polydat exists in the workload runtime. |
| §"Reification: runtime state → Polydat wire" | KEEP | nmbrs-runtime integration concern. |
| §"DSL Syntax" (lines 82–384) | **DELETE** | Now owned by [grammar.md §2 (productions)](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/grammar.md). Replace with a one-line pointer. |
| §"Bitwise Operations" (lines 385–418) | KEEP | Library-catalog reference; concrete operator behavior. |
| §"Const Expression Syntax" (lines 419–441) | **REDUCE** | Cross-reference [grammar.md G2 + expression_engine.md §3.1](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/expression_engine.md); keep only nmbrs-side use cases. |
| §"Type Inference Details" (lines 442–462) | **DELETE** | Owned by [grammar.md §3 type-inference rules](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/grammar.md). |
| §"Compilation Pipeline" (lines 463–500) | **REDUCE** | Cross-reference [graph_compiler.md §2 pipeline overview](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/graph_compiler.md). Keep only nmbrs-runtime-level invocation context. |
| §"Output Selection" (lines 501–515) | KEEP | nmbrs-runtime scope; how the activity selects which kernel output to consume. |
| §"Type System" (lines 516–571) | **REDUCE** | Cross-reference [composition_substrate.md T1](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md) + [grammar.md §3](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/grammar.md). Keep only the part listing nmbrs-runtime's use of PortType. |
| §"Node Contract" (lines 572–592) | **DELETE** | Owned by [composition_substrate.md §2 (slot contract)](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md). |
| §"Wiring Model" (lines 593–616) | **DELETE** | Owned by [composition_substrate.md §2 + runtime_model.md §1](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/runtime_model.md). |
| §"GK as Unified State Holder" (lines 617–637) | KEEP | nmbrs-runtime framing — why Polydat kernels hold scope state. |
| §"Incremental Invalidation" (lines 638–663) | **DELETE** | Owned by [runtime_model.md §3-§4 (R1, R2)](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/runtime_model.md). |
| §"GK Scope Model" (lines 664–688) | **DELETE** | Owned by SRD-13c + [composition_substrate.md L1/L2](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md). |
| §"Op-Level Bindings" | KEEP | nmbrs-runtime-specific. |
| §"Cursor Declarations" | KEEP | nmbrs-runtime-specific. |

**Estimated reduction:** ~450 lines DELETE + ~80 lines REDUCE. SRD-10 shrinks from 897 → ~350 lines, becoming an nmbrs-side integration doc that points at polydat for the substrate.

### SRD-11: Polydat Evaluation Model (498 lines)

Substantial overlap with runtime_model.md and
composition_substrate.md.

| Section | Disposition | Action |
|---|---|---|
| §"Program / State Split" | **DELETE** | Owned by [runtime_model.md §5 + composition_substrate.md L1](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/runtime_model.md). |
| §"Provenance-Based Invalidation" | **DELETE** | Owned by [runtime_model.md §2 (Dependency tracking) + §4 (Invalidation)](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/runtime_model.md). |
| §"Two Evaluation Lifecycles" | **REDUCE** | Cross-reference [composition_substrate.md L2 + grammar.md G2/G5](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md). Keep the host-visible behavioral distinction. |
| §"Const Binding Contract" | **DELETE** | Owned by [grammar.md G2 + composition_substrate.md L2](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/grammar.md). Plan A/B mechanism is polydat-internal. |
| §"Non-Deterministic Nodes" | **REDUCE** | Cross-reference [runtime_model.md D2 (purity declaration)](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/runtime_model.md). |
| §"Input Spaces" | **DELETE** | Owned by [composition_substrate.md §2 (slot contract)](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md). |
| §"Capture Context" | **REDUCE** | Cross-reference [composition_substrate.md L3](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md). Keep nmbrs-side capture integration. |
| §"Compilation Levels" | **DELETE** | Owned by [graph_compiler.md §6 (pipeline) + SRD-16](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/graph_compiler.md). |
| §"FiberBuilder" | KEEP | nmbrs-runtime integration surface. |
| §"Cursor-Driven Evaluation" | KEEP | nmbrs-runtime scope — how the activity cycle pump drives polydat. |

**Estimated reduction:** ~280 lines DELETE + ~60 lines REDUCE. SRD-11 shrinks from 498 → ~170 lines.

### SRD-12: Polydat Standard Library (359 lines)

Less overlap than 10/11; the library catalog itself stays
in SRD-12 because polydat doesn't have a library-catalog
design doc.

| Section | Disposition | Action |
|---|---|---|
| §"Wire Cost Classes" | KEEP | Library reference. |
| §"Node Categories" (39–292) | KEEP | Library catalog. **Gap:** polydat doesn't have a formal library-catalog doc; SRD-12 is the de facto reference. |
| §"Registration" + §"Node Registration" | **REDUCE** | Cross-reference [expression_engine.md §5.5 (virtual nodes)](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/expression_engine.md) for the factory-registration contract. Keep nmbrs-specific registration sites. |
| §"GK Modules" | **DELETE** | Owned by SRD-13. |
| §"Node Fusion" | **DELETE** | Owned by [graph_compiler.md §5](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/graph_compiler.md). |

**Estimated reduction:** ~60 lines. SRD-12 shrinks from 359 → ~300 lines.

### SRD-13: Polydat Modules (109 lines)

| Section | Disposition | Action |
|---|---|---|
| §"Module System" | **REDUCE** | Module definition syntax is owned by [grammar.md §2.4](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/grammar.md). Keep nmbrs-side module-resolution concerns (file paths, stdlib organization). |
| §"Compiler Diagnostic Event Stream" | KEEP | nmbrs-runtime-facing diagnostic API. |

**Estimated reduction:** minor. SRD-13 shrinks from 109 → ~80 lines.

### SRD-13b: Polydat Combination Modes (208 lines)

Largely framework material that polydat extended. Stays
mostly intact.

| Section | Disposition | Action |
|---|---|---|
| §"1. Inline" | KEEP | nmbrs-side terminology. |
| §"2. Scope composition" | **CROSS-REF** | Add pointer to [composition_substrate.md + SRD-13c](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md). |
| §"3. Subgraph" | KEEP | nmbrs-side terminology. |
| §"4. Reification" | KEEP | nmbrs-side terminology. |
| §"Retired terminology" | KEEP | Historical reference. |
| §"Quick reference" + §"Implications for pragmas/controls/metrics" | KEEP | nmbrs-side integration. |

**Estimated reduction:** none structural. Add ~3 cross-references.

### SRD-13c: Polydat Scope Model (891 lines)

Highest-overlap SRD. composition_substrate.md L-axioms +
graph_compiler.md CF-axioms are the load-bearing
formalisation now.

| Section | Disposition | Action |
|---|---|---|
| §"Principles" | **REDUCE** | The S/T/L pillars now formalise these principles. Cross-reference and keep the nmbrs-framing intro. |
| §"Scope Hierarchy" | **REDUCE** | Cross-reference [composition_substrate.md L1 + L4](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md). Keep nmbrs-runtime scope-tree integration. |
| §"No Flattening, No Duplication" | **DELETE** | Owned by [composition_substrate.md L1](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md). |
| §"Visibility Rules" | **REDUCE** | Cross-reference [grammar.md G3 (scope-chain transparency) + composition_substrate.md L1](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/grammar.md). Keep workload-author-visible behavior. |
| §"Mutability Rules" | **REDUCE** | Cross-reference [composition_substrate.md L4 + SRD-13f](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md). Keep nmbrs-side `shared` / `const` author guidance. |
| §"Scope Lifecycle for for_each" | **REDUCE** | Cross-reference [graph_compiler.md §4 (Context Fusion)](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/graph_compiler.md). Keep the workload-author lifecycle story. |
| §"Implementation via Existing Mechanisms" | **DELETE** | Owned by [graph_compiler.md CF1-CF4 + composition_substrate.md S2](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/graph_compiler.md). |
| §"Variable Partitioning" | **DELETE** | Owned by [composition_substrate.md L2 + grammar.md G5](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md). |
| §"Syntax Summary" | **DELETE** | Owned by [grammar.md §2.3](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/grammar.md). |
| §"How It Works: Plugging Graphs Together" | **DELETE** | Owned by [graph_compiler.md §4 (Context Fusion)](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/graph_compiler.md). |
| §"What This Does NOT Change" | KEEP | nmbrs-side framing. |
| §"Open Design Issue" | KEEP | nmbrs-side discussion. |
| §"Design Rationale" | KEEP | Historical context. |

**Estimated reduction:** ~430 lines DELETE + ~180 lines REDUCE. SRD-13c shrinks from 891 → ~280 lines.

### SRD-13d: Op-template Polydat Scope Layer (779 lines)

Mostly nmbrs-runtime-specific (op templates aren't a polydat
concept). Keep with cross-references to substrate.

| Section | Disposition | Action |
|---|---|---|
| §"1-3" + §"4. Realisation lifecycle" | KEEP | nmbrs-runtime-specific. |
| §"5. Walking parent-kernel reference" | **CROSS-REF** | Add pointer to [composition_substrate.md L1 + graph_compiler.md CF1](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md). |
| §"6. Proving-out test suite" | KEEP | nmbrs-runtime tests. |
| §"7. Structural rules carried forward from SRD-13c" | **REDUCE** | Cross-reference SRD-13c (which now points at polydat). |
| §"8-9" | KEEP | nmbrs-runtime-specific. |

**Estimated reduction:** ~30 lines. SRD-13d shrinks from 779 → ~750 lines.

### SRD-13e: Scope-as-Module Refinement (678 lines)

DESIGN doc; reframe as integration spec with polydat substrate.

| Section | Disposition | Action |
|---|---|---|
| §"What this SRD covers" | **REWRITE** | Reframe: nmbrs-runtime uses polydat's `kernel/subcontext/` (per SRD-67); this SRD specifies the typed `ScopeModule` interface on top. |
| §"Why this SRD now" | KEEP | Historical motivation. |
| §"1. The contract surface" | **REDUCE** | Cross-reference [composition_substrate.md L1 + L4 + SRD-67](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md). Keep nmbrs-side typed-module shape. |
| §"2-4" | KEEP | nmbrs-side mechanism. |
| §"5. Migration plan" | KEEP | nmbrs-side execution. |

**Estimated reduction:** ~80 lines. SRD-13e shrinks from 678 → ~600 lines.

### SRD-13f: Cross-Scope Wire Materialization (944 lines)

The most detailed of the 13-family. composition_substrate L4
formalises the read/write invariant; graph_compiler CF3
formalises the gradient honoring.

| Section | Disposition | Action |
|---|---|---|
| §"What this SRD covers" | KEEP | Scoping intro. |
| §"Architectural model" | **REDUCE** | Cross-reference [composition_substrate.md L4 (write-through) + graph_compiler.md CF3](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md). Keep the cell-classification visual. |
| §"Materialization gradient" | **REDUCE** | Cross-reference [graph_compiler.md CF3 (gradient honoring)](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/graph_compiler.md). Keep nmbrs-side classification details. |
| §"Wire-reference classification (synthesizer rule)" | KEEP | nmbrs-runtime synthesizer is the consumer. |
| §"How this differs from what's coded today" | KEEP | Implementation status. |
| §"Plan to true-up" | KEEP | nmbrs-side work. |
| §"Open questions / deferred" | KEEP | Future work. |
| §"Summary" | **REDUCE** | Cross-reference polydat axioms. |

**Estimated reduction:** ~150 lines. SRD-13f shrinks from 944 → ~800 lines.

### SRD-14: Polydat Config Expressions (170 lines)

Direct overlap with expression_engine.md.

| Section | Disposition | Action |
|---|---|---|
| §"Expression Syntax" | **DELETE** | Owned by [expression_engine.md §3.1 (eval_const_expr)](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/expression_engine.md). |
| §"Resolution Order" | KEEP | nmbrs-side param-resolution order is host-specific. |
| §"Const Expression Evaluation" | **DELETE** | Owned by [expression_engine.md §3.1 + grammar.md G2](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/expression_engine.md). |
| §"Config Value Types" | **REDUCE** | Cross-reference [composition_substrate.md T1](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md). |
| §"Param Substitution Interaction" | KEEP | SRD-21 ownership; nmbrs-side. |
| §"Error Handling" | **REDUCE** | Cross-reference [expression_engine.md §6 (EmbeddingError)](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/expression_engine.md). |
| §"Implementation State" | DELETE | Stale tracking. |
| §"What This Replaces" | KEEP | Historical context. |

**Estimated reduction:** ~60 lines. SRD-14 shrinks from 170 → ~110 lines.

### SRD-15: Strict Mode (697 lines)

Mostly nmbrs-side concern; small overlap with
expression_engine §5.1.3 (opt-in strict contract).

| Section | Disposition | Action |
|---|---|---|
| Most sections | KEEP | nmbrs-side strict-mode policy is host-specific. |
| Any section discussing typed-embedding strict mode | **CROSS-REF** | Pointer to [expression_engine.md §5.1.3](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/expression_engine.md). |

**Estimated reduction:** minor. ~20 lines.

### SRD-16: Polydat Engines (225 lines)

Overlap with graph_compiler.md §6 (pipeline including engine
selection) and §5.3 (Node Fusion polyfills).

| Section | Disposition | Action |
|---|---|---|
| §"Compilation Levels" | **REDUCE** | Cross-reference [graph_compiler.md §2 pipeline + §6 ordered composition](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/graph_compiler.md). Keep nmbrs-runtime invocation. |
| §"Provenance Optimization" | **DELETE** | Owned by [runtime_model.md §2 (Dependency tracking) + §4 (Invalidation)](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/runtime_model.md). |
| §"Automatic Selection Heuristic" | KEEP | Engine selection is a polydat-internal concern; details stay here for now (no polydat doc owns it). |
| §"Type System" | **DELETE** | Owned by [grammar.md §3 + composition_substrate.md T1](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/grammar.md). |

**Estimated reduction:** ~80 lines. SRD-16 shrinks from 225 → ~145 lines.

### SRD-16b: Polydat JIT Wiring (351 lines)

Cranelift-specific; mostly polydat-internal but the
boundary contract is here. graph_compiler.md T3 references
it as authority.

| Section | Disposition | Action |
|---|---|---|
| All sections | KEEP | JIT internals are polydat-internal but their detailed contract (setjmp/longjmp, extern-helper table, fallback paths) doesn't have a polydat design doc. SRD-16b IS the authority. **Gap:** consider whether to migrate this to a polydat-side `jit_boundary.md` design doc. |

**Estimated reduction:** none. **Gap noted.**

### SRD-67: Parent-gated Subcontext Construction (1030 lines)

The walled-off construction API. composition_substrate.md
references it as the chokepoint enforcer for L1/L4.

| Section | Disposition | Action |
|---|---|---|
| §"What this SRD specifies" | KEEP | Scoping. |
| §"Vocabulary" | KEEP | nmbrs-side terminology. |
| §"The construction protocol" | KEEP | Polydat-internal but no polydat doc owns the parent-gated protocol in depth. SRD-67 IS the authority. **Possible gap:** consider migrating to polydat-side `subcontext_construction.md`. |
| §"Compile once, spawn once, fiber-state separately" | **CROSS-REF** | Add pointer to [composition_substrate.md L1](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md). |
| §"Named-child registry" | KEEP | nmbrs-runtime integration. |
| §"Cross-binding rules" | **REDUCE** | Cross-reference [composition_substrate.md L1+L4 + graph_compiler.md CF3](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md). |
| §"Walled-off invariant" | KEEP | The chokepoint contract — load-bearing for substrate enforcement. |
| §"What disappears" | KEEP | Historical context. |
| §"Lifecycle boundary contract" | **REDUCE** | Cross-reference [graph_compiler.md CF4 (synthesis once per scope-init)](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/graph_compiler.md). |
| §"Composition with SRD-13e" | KEEP | nmbrs-side cross-reference. |

**Estimated reduction:** ~100 lines. SRD-67 shrinks from 1030 → ~930 lines.

### SRD-74: None Propagation (302 lines)

Owned at the polydat layer per composition_substrate.md T1
+ runtime_model.md D1. The SRD's specific rules are useful
reference but the high-level model is polydat's.

| Section | Disposition | Action |
|---|---|---|
| §"Motivation" | KEEP | nmbrs-side motivation. |
| §"Three orthogonal rules" | KEEP | Concrete rule list useful as reference; cross-reference [composition_substrate.md T1 + runtime_model.md D1](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md). |
| §"Interaction with set: and the GK-grammar invariant" | KEEP | nmbrs-runtime workload integration. |
| §"Conditional-shadow semantics for const" | KEEP | Workload-author behavior. |
| §"Test contract" | KEEP | nmbrs-side tests. |
| §"Phased delivery" | DELETE | Stale tracking. |
| §"Why this is safe" + §"See also" | KEEP | Discussion + cross-refs. |

**Estimated reduction:** ~30 lines.

---

## Gap analysis

Three categories of gaps surfaced during the audit.

### Gap 1: polydat surfaces without design-doc coverage

**G1a — Function-node library catalog.** polydat's
`library/` directory contains 37 function-node modules
(arithmetic, string, hash, weighted, etc.) plus the
`sampling/` subtree (alias tables, ICD, LUT, metashift)
and `support/` (cache, audit). There is **no polydat-side
design doc** for the library. SRD-12 is the de facto
catalog, but it's structured as a reference (what nodes
exist) rather than a design (what the catalog's
organizational principle IS). **Recommendation:** the
"sampling primitives algebra" focal-point candidate
named in the earlier scoping discussion would address
the sampling subtree; a separate doc could cover the
function-node taxonomy.

**G1b — JIT boundary semantics.** SRD-16b carries the
detailed contract (setjmp/longjmp, extern-helper table,
fallback paths). graph_compiler.md T3 references it as
authority. **Recommendation:** consider migrating SRD-16b
content into a polydat-side `jit_boundary.md` (since the
content is purely about polydat internals); or leave SRD-16b
in place but mark it as the polydat-internal contract.

**G1c — Parent-gated subcontext construction protocol.**
SRD-67 carries the detailed protocol; composition_substrate
and graph_compiler reference it as the construction-tier
chokepoint enforcer. **Recommendation:** same options as
G1b — either migrate to polydat-side or mark as polydat-
internal authority.

**G1d — Engine selection heuristic.** SRD-16's "Automatic
Selection Heuristic" section is the only doc for how
polydat's `compile::select` chooses between P1/P2/P3.
graph_compiler.md §2 mentions engine selection as a
pipeline pass but doesn't formalize the heuristic.
**Recommendation:** likely fine as-is; the heuristic is
narrowly scoped.

### Gap 2: SRD claims polydat doesn't yet implement

**No critical gaps found.** The audit confirms that polydat
implements every load-bearing claim across the affected
SRDs. The earlier `spec_code_audit.md` (now deleted)
verified each axiom against code; this audit doesn't find
SRD claims that fall through to polydat without
implementation.

The minor exceptions are already documented as open
questions in the SRDs themselves:
- SRD-13e Migration plan: incremental; some phases still in flight.
- SRD-13f Plan to true-up: incremental; some refinements still pending.
- SRD-74 Phased delivery: largely done but some Rule-2 work remains.

### Gap 3: nmbrs-side coverage holes that polydat doesn't backfill

**G3a — Workload→polydat compilation bridge.** SRD-20
(Workload Model) defines the YAML structure. SRD-21
(Parameters) defines bind-point resolution. The bridge
from a parsed workload to a polydat kernel (what
nmbrs-workload does when it produces a `compile_polydat` call
sequence) doesn't have a dedicated design doc. The code
lives in `crates/nmbrs-workload/src/` but the contract between
nmbrs-workload and polydat-as-substrate is implicit.
**Recommendation:** lower priority; the bridge is
stable and small, and the contract is "produce valid
GK source text and invoke `compile_polydat`."

**G3b — Capture-points cross-tier integration.** SRD-34
(Capture Points), SRD-69 (Capture Semantics, draft),
SRD-70 (Capture Paths, draft) cover the nmbrs-runtime
side. composition_substrate.md L3 references captures
as the cycle-time-binding mechanism. The cross-tier
integration — how captures cross from op-execution into
the polydat slot contract — is covered piecemeal across
these docs but lacks a single canonical statement.
**Recommendation:** add a section to expression_engine.md
§5.7.3 (capture-aware embedding patterns) or to
composition_substrate.md L3 that explicitly states the
cross-tier integration.

---

## Execution plan

A staged reduction over ~6 nmbrs-side pushes, in order:

### Push A1 — SRD-10 reduction (largest single SRD edit)

- DELETE: DSL Syntax sections (§§82–384), Type Inference
  Details, Node Contract, Wiring Model, Incremental
  Invalidation, Polydat Scope Model.
- REDUCE: Compilation Pipeline, Type System, Const
  Expression Syntax.
- Add cross-references per the table above.
- Verify: SRD-10 shrinks from 897 → ~350 lines.

### Push A2 — SRD-11 reduction

- DELETE: Program/State Split, Provenance-Based
  Invalidation, Const Binding Contract, Input Spaces,
  Compilation Levels.
- REDUCE: Two Evaluation Lifecycles, Non-Deterministic
  Nodes, Capture Context.
- Verify: SRD-11 shrinks from 498 → ~170 lines.

### Push A3 — SRD-13c reduction

- DELETE: No Flattening, Implementation via Existing
  Mechanisms, Variable Partitioning, Syntax Summary, How
  It Works: Plugging Graphs Together.
- REDUCE: Principles, Scope Hierarchy, Visibility Rules,
  Mutability Rules, Scope Lifecycle for for_each.
- Verify: SRD-13c shrinks from 891 → ~280 lines.

### Push A4 — SRD-13f, SRD-13d, SRD-13e reduction

- Per-section cross-references and reductions as tabled.
- Verify: ~260 lines reduction across the three.

### Push A5 — SRD-12, SRD-13, SRD-14, SRD-16 reduction

- Per-section reductions; mostly small.
- Verify: ~250 lines reduction across the four.

### Push A6 — SRD-67, SRD-74, SRD-15, SRD-16b cross-references + final cleanup

- Add cross-references; small reductions.
- Decide on G1b/G1c migration to polydat side (or leave
  SRDs as authority with explicit polydat-internal
  marker).

### Net effect

| Metric | Before | After | Reduction |
|---|---|---|---|
| SRD-10 | 897 | ~350 | 547 |
| SRD-11 | 498 | ~170 | 328 |
| SRD-12 | 359 | ~300 | 59 |
| SRD-13 | 109 | ~80 | 29 |
| SRD-13c | 891 | ~280 | 611 |
| SRD-13d | 779 | ~750 | 29 |
| SRD-13e | 678 | ~600 | 78 |
| SRD-13f | 944 | ~800 | 144 |
| SRD-14 | 170 | ~110 | 60 |
| SRD-15 | 697 | ~680 | 17 |
| SRD-16 | 225 | ~145 | 80 |
| SRD-67 | 1030 | ~930 | 100 |
| SRD-74 | 302 | ~275 | 27 |
| **Total** | **7,579** | **~5,470** | **~2,100** |

A ~28% reduction in nmbrs-side SRD content, with the
removed material now formally owned by polydat docs and
cross-referenced from the SRDs.

---

## Cross-reference convention

When an SRD section is REDUCED or DELETED in favor of a
polydat doc, replace the section with:

```markdown
## [Original title]

**This section is now owned by
[polydat's <doc>.md](../path/to/polydat/docs/design/<doc>.md)
§<section>.** Polydat's <axiom-suite> formalises the
mechanism this section described.

[Optional: 1-2 paragraphs of nmbrs-side framing — why nmbrs
cares, how nmbrs invokes the polydat mechanism — but not
the mechanism itself.]
```

For cross-references inside a kept section, use the
inline form:

```markdown
... per [composition_substrate.md L2](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md) ...
```

Either form is recognisable as "polydat owns the
definition; this is nmbrs-side framing."

---

## What's NOT in scope

- The comprehension-related SRDs (18b/c/d/e, 78) — polydat
  has its own `comprehension_forms.md` treatment; the SRDs
  were already cross-referenced to it during the
  comprehension cutover. No additional reduction needed.
- SRDs about workload-level concerns (20, 21, 22, 30-44,
  60-66, 71-77) — polydat-adjacent in places but not
  duplicative of polydat design docs.
- SRD-02 (Concurrency Model) — runtime_model.md L1
  references it; the SRD is host-side concurrency
  contract, not polydat-substrate.

---

## Open questions for the audit

1. **G1b/G1c migration decision** — do we migrate SRD-16b
   (JIT wiring) and SRD-67 (parent-gated construction)
   content to polydat-side design docs, or leave them in
   `docs/SRD/` with explicit polydat-internal markers?
2. **Cross-reference style** — the convention above uses
   `[doc.md §X](../path)` form. Some SRDs use a different
   convention (`[name](url)` without §). Pick one and
   apply uniformly during reduction.
3. **Reduction execution order** — Push A1 (SRD-10) is
   the biggest single edit. Land it first to validate the
   reduction approach, or stage smaller pushes first to
   build pattern familiarity?
