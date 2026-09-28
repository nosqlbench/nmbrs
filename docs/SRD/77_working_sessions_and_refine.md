# SRD-77 — Working Sessions, Executions, and the `refine` Verb

**Status:** DRAFT — design for the operator-facing
"refine the session by running new / changed phases"
workflow + the underlying entity-model split (Session vs
Execution). No code lands until reviewed against SRD-44
(checkpoint event log) + SRD-76 (PhaseOutcome /
SessionDisposition).

**Owner:** nmbrs-runtime (session model, runner,
checkpoint-resume planner), nmbrs-metrics (sqlite
schema), nmbrs (CLI verbs + flags), workloads (consumers
of the new verb).

**Cross-refs:**
- [SRD-44](44_workload_checkpointing.md) §"Phase identity"
  — the identity tuple `(yaml_path, coords, phase_hash)`
  is the load-bearing primitive this SRD re-uses for
  the coverage / overlap question.
- [SRD-76](76_phase_outcome_disposition.md) — structured
  `PhaseOutcome` per phase; SRD-77 extends every
  outcome row with an `execution_id` foreign key.
- [SRD-46](46_reports.md) — `nmbrs report` gains an
  `--execution=<id>` filter so reports can scope to a
  single execution or aggregate across the whole
  session.
- [SRD-15](15_strict_mode.md) — strict mode forbids the
  bare `--session` (must pick `--new-session` or
  `--resume-session` explicitly).
- [SRD-63](63_status_readouts.md) — readout context
  gains `execution_id`-aware filters; the realtime
  status surface shows which execution is in flight.

---

## What this SRD covers

The operator-facing problem the verb solves: **"I modified
my workload — maybe added new sweep cells, maybe changed an
op's bindings, maybe both. Run only what's new or changed;
keep my existing data."**

Today `nmbrs run` always creates a fresh session dir and
re-runs everything. `nmbrs resume` reruns a session that
was interrupted, matching prior identity to skip done
phases — but it doesn't tolerate workload changes and
doesn't accept new phases beyond the prior pre-map.

`refine` closes this gap and brings along three load-
bearing entity-model changes that have been quietly
needed by other SRDs (44, 46, 63, 76):

1. **Working sessions** — a session is a persistent
   container, not a single invocation's scratch space.
2. **Executions** — within a session, each `nmbrs <verb>`
   invocation is a distinct **execution** with its own
   identity, workload-yaml snapshot, and outcome roll-up.
3. **Universal session-selection flags** — `--new-session`,
   `--resume-session`, `--session` (basic) trio across every
   nmbrs verb that operates on a session, with strict mode
   forbidding the bare ambiguous form.

---

## The verb: `refine`

The metaphor is topology: **refining an open cover** by
adding finer pieces that subordinate to the existing cover.
Each `refine` execution adds finer pieces to the session's
coverage of the parameter space — new phases for new cells,
re-runs for phases whose definition changed.

CLI shape:

```
nmbrs refine [session selection] [scope] workload=<file>
```

Without any session selection: warn if no `sessions/latest`
exists, then create a new session and run all phases
(equivalent to `nmbrs run` for the first execution). With an
existing target session: layer a new execution onto it
following the `--scope` selector.

### Comparison across verbs

| Verb | Session | Workload version | Phases run |
|---|---|---|---|
| `run` | always fresh | the one passed | all |
| `resume` | re-attach to existing | must match prior | incomplete + missing |
| `refine` | re-attach OR fresh | may differ from prior | per `--scope`: missing (default), changed, or all |

`resume` is the strict "I crashed mid-run, redo what didn't
finish" verb. `refine` is the editorial "I edited the
workload, fill in or replace what changed" verb. The two
are NOT equivalent: `resume` insists on workload-identity
match (won't run a phase that didn't exist in the prior
pre-map); `refine` happily runs new phases.

---

## Entity model — Session vs Execution

Today's model has a single tier:
```
Session   ←  one invocation of nmbrs
```

The new model splits into two tiers:
```
Session   ←  persistent container
└── Executions[]   1..N executions, in temporal order
    └── PhaseOutcomes[]   scoped to (session_id, execution_id)
```

### `Session`

- `session_id`: stable identifier for the session's lifetime.
  Used as the dir basename under `sessions/`. Default form
  unchanged from today (`<scenario>_<timestamp>`); the new
  flag trio can override.
- `created_at`: timestamp at session birth.
- `output_dir`: filesystem path to the session dir.
- The session is the unit of `sessions/latest` symlink, the
  unit of `nmbrs report`, the unit of `nmbrs replay`.

### `Execution`

- `execution_id`: monotonic per-session sequence (1, 2, 3,
  ...). The current execution is `session.executions.len()`
  at start of invocation; the next is +1.
- `verb`: `run` | `resume` | `refine` — how this execution
  was launched.
>> Let's shorten 'execution_id' to 'exec_id' and 'session_id' to 'sess_id'
- `started_at`, `ended_at`, `disposition`
  (`SessionDisposition` per SRD-76 — but renamed
  `ExecutionDisposition` since it now scopes to one
  execution, not the whole session — see §"Naming
  rationalization" below).
- `workload_yaml_snapshot`: the full workload yaml as it
  was at this execution's start. Stored verbatim so an
  operator can reconstruct exactly what this execution ran.
- `cli_params_snapshot`: CLI overrides verbatim (one
  `key=value` per line).
- `scope`: which `--scope=` setting was in effect
  (`missing` / `changed` / `all`).

A session's executions form the **cardinal history** —
what was attempted, when, with what workload version, and
how it disposed.

### Naming rationalization for SRD-76

SRD-76 used `SessionDisposition` as the binary pass/fail
projection across "the whole session". With the new
two-tier model, the canonical projection is at the
EXECUTION level — each execution disposes to
success/failure independently. The session as a whole
has its own projection (any failed execution → session
failure? all-success execution wins out?).

SRD-77 proposes:
- `ExecutionDisposition` (renamed from `SessionDisposition`)
  — binary pass/fail of one execution's `PhaseOutcome` set.
- `SessionDisposition` (new) — binary pass/fail of the
  ENTIRE session's history. **Rule**: failure if the
  most-recent execution failed; success otherwise. Earlier
  failures don't propagate forward — the operator's intent
  with a follow-up `refine` is "let me make this pass" and
  a subsequent success should mark the session healthy.

The SRD-76 SceneTree `session_disposition()` method
becomes `current_execution_disposition()` for the in-flight
execution and `session_disposition()` keeps its name but
walks all executions, applying the most-recent-wins rule.

---

## Session selection — the universal flag trio

Three flags, every nmbrs verb that operates on a session
respects them:

| Flag | Semantic | Behavior on existing | Behavior on missing |
|---|---|---|---|
| `--new-session=<name>` | "Create new" | ERROR (name in use) | create + run |
| `--resume-session=<name>` | "Resume existing" | re-attach | ERROR (no such session) |
| `--session=<name>` | Basic / opportunistic | re-attach | create + run |
| *(no flag)* | Implicit (verb-dependent default) | per verb | per verb |

Strict mode (`--strict` / `NMBRS_STRICT=1`): the bare
`--session` is rejected. The operator must pick
`--new-session` or `--resume-session` explicitly. This
forecloses the "I thought it would resume but it created
fresh" / "I thought it would create fresh but it
re-attached" ambiguity that the basic flag invites.

### No-flag defaults per verb

| Verb | No-flag behavior |
|---|---|
| `run` | always fresh; auto-generates a session name. To target a specific name use `--new-session`. |
| `resume` | reads `sessions/latest`; errors if absent. To target a different session use `--resume-session`. |
| `refine` | reads `sessions/latest` (warns + falls back to `run`-like behavior if absent). To create a new session use `--new-session`. |

### Implicit `--session` for `refine` with no flag and no `latest`

When `refine` is invoked with no session flag AND
`sessions/latest` doesn't exist:

```
nmbrs refine workload=…
warning: no `sessions/latest` — creating a new session.
         For non-ambiguous behavior, use `--new-session=<name>`
         or `--resume-session=<name>`.
```

The warning is the operator-visible note that they got the
first-execution-of-a-new-session form rather than the
intended layer-onto-existing form. Strict mode promotes the
warning to an error (forces the explicit flag).

---

## `sessions/` directory rename

`logs/` → `sessions/`. The directory holds session state
(metrics.db, session.log, checkpoints, reports), not just
logs.

Convenience symlinks at the new path:
- `sessions/latest` → most-recent session dir
- `sessions/metrics.db` → `latest/metrics.db`
- `sessions/session.log` → `latest/session.log`

**Migration**: this is a flag-day rename. Existing `logs/`
directories are NOT auto-migrated; the release note
documents a one-shot `mv logs sessions` for operators who
care about retention.

---

## Phase fingerprint

The "did this phase change?" oracle for `--scope=changed`
is the existing infrastructure:

- `PhaseIdentity { yaml_path, coords, phase_hash }` (SRD-44)
- `checkpoint::compose_phase_hash` — the composed provenance
  hash (SRD-44 §"What the hash covers"): the ancestor-chain
  instance hash (workload-params module → workload root →
  intermediate scopes) composed with the canonical
  phase-config digest (ops, bindings, cycles — every
  declared `WorkloadPhase` field).

SRD-77 lifts this from "stored in the checkpoint event
log" to "stored on `PhaseOutcome` as well" — so
`refine --scope=changed` can compare the prior outcome's
hash against the freshly-computed hash without needing the
checkpoint log to be the source of truth.

The provenance hash covers:
- Workload-param VALUES (const slots on the session-level
  params module in the ancestor chain — coverage is
  whole-module: any param change flips every phase's hash;
  see SRD-44 for why that direction is the safe one)
- Workload-root and intermediate scope programs (top-level
  bindings, for_each / for_combinations / bindings scopes)
- The phase's declared configuration, op statement templates
  included (the config digest)

What's NOT in the hash:
- Workload-level metadata outside the phase declarations
  (description text, comments, `report:` block)
- Session plumbing flags (`--session*`, `tui=`, log levels)

Any change to a load-bearing declaration (a binding edit, a
param value, a changed statement in an op template, a
cycle-count change) flips the hash and `--scope=changed`
re-runs the affected phases.

---

## `--scope` selector

Three modes, default `missing`:

| Mode | Phases run |
|---|---|
| `missing` (default) | identity has no prior outcome in this session (any execution) |
| `changed` | identity matches but `phase_hash` differs from the most-recent prior outcome. Includes `missing`. |
| `all` | every phase per the workload. New outcomes overwrite prior. |

`changed` is the typical follow-up: "I edited my workload's
op template; re-run the affected phases without redoing the
unrelated ones."

`all` is the operator's escape hatch: full re-run, prior
outcomes preserved as history under their `execution_id`
but no longer the latest answer.

---

## Removed-phase policy

If a workload version removes a phase that exists in prior
executions (operator dropped a sweep axis, removed a
phase from the scenario tree):

Default: **error** — refuses to proceed.

```
nmbrs refine: workload removes 1 phase that has prior
outcomes in this session:
  phase 'ann_query' @ (k=10, limit=20, profile=default)
    (from execution 2, completed 2026-05-27 14:30:00)

Pass `--on-removed=keep` to silently retain the prior
outcome (no work, no error), or `--on-removed=drop` to
delete the prior outcome (with confirmation prompt).
```

Options:
- `--on-removed=error` (default): refuse to proceed
- `--on-removed=keep`: retain prior outcomes; don't run the
  removed phase
- `--on-removed=drop`: delete prior outcomes for removed
  phases (interactive confirm unless `--force-drop`)

Default-error prevents accidental data loss from a typo or
accidental axis-trim. The keep / drop policies are explicit
opt-ins.

---

## SQLite schema impact

### New table: `executions`

```sql
CREATE TABLE executions (
    execution_id INTEGER PRIMARY KEY AUTOINCREMENT,
    verb TEXT NOT NULL,              -- 'run' | 'resume' | 'refine'
    scope TEXT,                       -- 'missing' | 'changed' | 'all' (NULL for run/resume)
    started_at_nanos INTEGER NOT NULL,
    ended_at_nanos INTEGER,           -- NULL while in flight
    disposition TEXT,                 -- ExecutionDisposition; NULL while in flight
    workload_yaml_snapshot TEXT NOT NULL,
    cli_params_snapshot TEXT NOT NULL
);
```

A row is inserted at execution start; `ended_at_nanos` +
`disposition` are updated at execution end via the SRD-76
shutdown guard.

### `execution_id` column on every per-phase table

Tables that today carry per-phase rows (per SRD-76):
- `phase_outcomes`
- `phase_errors`

Plus tables from other SRDs that need the per-execution
scope:
- (Checkpoint event log if migrated from JSONL to sqlite;
  see SRD-44 — out of scope here)
- Metrics rows (every metric sample carries the
  `execution_id` of the execution that produced it)

Schema-update path: an `ALTER TABLE` migration runs at
session open. Old sessions without the column read as
`execution_id = 1` (the legacy single-execution session).

### Composite primary keys

The SRD-76 primary keys gain `execution_id`:

```sql
-- before SRD-77
PRIMARY KEY (phase_name, phase_labels, ended_at_nanos)

-- after SRD-77
PRIMARY KEY (execution_id, phase_name, phase_labels)
```

The `ended_at_nanos` is no longer needed for uniqueness
because two different runs of the same phase live under
different `execution_id`s.

---

## Migration plan (pushes)

Each push leaves the tree green and the operator-visible
surface usable.

### Push 1 — `sessions/` rename + flag trio

- Rename `logs/` references to `sessions/` everywhere
  (runner.rs path constants, session.rs, replay, report,
  docs, tests).
- Add `--new-session` / `--resume-session` to the CLI
  spec; basic `--session` keeps its current semantic but
  warns under strict mode.
- Test: existing workloads keep running unchanged using
  the basic flag.

### Push 2 — Execution entity

- New `Execution` struct in `crates/nmbrs-runtime/src/session.rs`
  alongside `Session`.
- `executions` sqlite table with the schema above.
- Lifecycle: at runner start, INSERT a row; at shutdown
  (via the SRD-76 guard), UPDATE the row with end-time +
  disposition.
- `execution_id = 1` is the default for legacy sessions.

### Push 3 — Phase outcomes gain `execution_id`

- `PhaseOutcome` struct (SRD-76) gains `execution_id: u64`.
- Sqlite schema migration: ALTER TABLE adds column with
  default = 1.
- SceneTree's `set_phase_outcome` records the
  current-execution's id.
- `session_disposition()` updates: most-recent-execution
  wins.

### Push 4 — `nmbrs refine` verb + `--scope` + removed-phase

- New verb registered in cli_spec.
- Pre-map walker runs as today; the resume planner gains
  a "compare against any prior outcome in the session"
  pass.
- For `--scope=changed`: compare freshly-computed
  `phase_hash` against the most-recent prior outcome's
  hash; skip when equal.
- Removed-phase detection: walk all prior outcomes; phases
  not present in this pre-map fire the removed-phase
  policy.

### Push 5 — `nmbrs report --execution=<id>` filter

- `nmbrs report` gains `--execution=<id>` to scope plots /
  tables to one execution.
- Default behavior (no filter): aggregate across all
  executions in the session.
- Same flag on `nmbrs replay`.

### Push 6 — Workload-author surfaces + tests

- Documentation in `docs/guide/`.
- Integration tests: a small workload, two `refine`
  invocations with axis changes between, verify
  scope=missing / scope=changed / scope=all behaviors.

---

## Invariants

1. **Phase identity equality is structural.** A phase is
   "the same" iff `(yaml_path, coords, phase_hash)` all
   match. The hash is the load-bearing piece — operator-
   visible name changes that don't affect the program
   chain (description text edits) leave the hash stable.
2. **Executions are append-only.** No execution row is
   ever deleted or rewritten. Updates (end-time,
   disposition) target only the in-flight row.
3. **Phase outcomes are append-only within an execution.**
   Two executions producing different outcomes for the
   same phase identity coexist; the most recent wins
   for "current state" queries, but both rows survive
   for cardinal-history queries.
4. **`refine` never destroys data without `--on-removed`
   opt-in.** Default-error semantics force the operator
   to confirm any data-removal intent.
5. **`sessions/latest` always points at a valid session
   directory.** Stale / broken latest symlinks are an
   operational bug (separate work).

---

## Open questions

- **Cross-execution checkpoint logs.** SRD-44's
  `checkpoint.jsonl` is currently single-stream. Multiple
  executions into the same session need either one
  append-only log with execution_id tags or per-execution
  files. Likely the former (simpler to query); confirm
  before Push 4.
- **Workload yaml drift detection.** If the operator runs
  `refine` with a `workload=` that's TOTALLY different
  from prior executions (different `scenarios:` block, no
  phases in common at all), is that an error or a silent
  full-re-run? Argue for warning + proceed; the operator
  may legitimately be reusing a session dir for a new
  experiment.
- **`refine` interacts with `for_each` extending in the
  middle of the cross product.** The user might add a NEW
  sweep value (e.g. `mnc in 8, 128, 512` where `512` is
  new). The pre-map walker enumerates the FULL cross
  product; `--scope=missing` filters to those not yet
  outcomes. Confirm this is the desired semantic vs. some
  per-axis "extend only the new value" mode.

---

## See also

- SRD-44 — Checkpoint event log + Phase identity
- SRD-46 — Reports (gains `--execution=<id>` filter)
- SRD-63 — Readouts (execution_id-aware context)
- SRD-76 — PhaseOutcome / disposition (extended with
  `execution_id` per Push 3)
- `docs/design/working_session_model.md` — companion
  design-discussion doc tracking the rationale behind
  the entity-model split as it evolves
