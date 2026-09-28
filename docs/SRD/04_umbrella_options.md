# SRD-04 — Umbrella options: scalable, multi-detail flag form

**Status:** normative
**Owner:** runtime / CLI
**Implementation:** `crates/nmbrs-runtime/src/session.rs::parse_session_kv` (pilot)

---

## Why

Some configuration surfaces have a small set of related
sub-options that the operator wants to specify together,
flexibly, without typing five separate flags. Each sub-option
also deserves its own well-named long-form flag for scripts
and clarity-in-help.

The umbrella-options pattern gives both: one short umbrella
flag for everyday use, plus full long-form flags for
unambiguous expression. They share the same domain of
configuration; what the operator types is a question of
preference and verbosity.

`--session` is the first surface to adopt this. Other
surfaces should follow the same rules.

---

## Rule 1 — One umbrella flag, one shape

The umbrella flag takes a single argument: a comma-separated
list of items. Each item is one of:

- A `key:value` pair (e.g. `name:foo`).
- A bare token shortcut (e.g. `restart`) that aliases to a
  specific kv pair.

```
--session 'keep:42,name:sessname42,path:sessions/dir/SESSION,reuse:resume'
```

Whitespace around commas, keys, and values is trimmed.
Empty items (e.g. `,,`) are skipped. Last-wins on repeated
keys.

---

## Rule 2 — Each kv key has a long-form flag

Every key recognised by the umbrella has a corresponding
`--<flag>-<key>` long-form flag. Setting them is equivalent.

| Umbrella `--session 'k:v'` | Long-form flag      |
| -------------------------- | ------------------- |
| `name:foo`                 | `--session-name=foo`|
| `path:foo`                 | `--session-path=foo`|
| `reuse:restart`            | `--session-reuse=restart` |
| `keep:42`                  | `--session-keep=42` |
| `shelflife:4w`             | `--session-shelflife=4w` |

Long-form flags accept both `=value` and space-separated
`<flag> <value>` shapes.

**Precedence**: long-form flag wins over umbrella. Both win
over env. Default last.

---

## Rule 3 — Bare tokens are shortcuts

Some keys have an enum value-set small enough that the value
itself is a recognisable shortcut. The umbrella accepts those
values as bare tokens; the parser maps each to its
canonical key:value form.

For `--session`:

| Bare token | Equivalent     |
| ---------- | -------------- |
| `restart`  | `reuse:restart`|
| `resume`   | `reuse:resume` |
| `error`    | `reuse:error`  |

```
--session 'restart,name:baseline'
```

is equivalent to

```
--session 'reuse:restart,name:baseline'
```

is equivalent to

```
--session-reuse=restart --session-name=baseline
```

Bare tokens belong to a single key. A token can never be
ambiguous between two umbrellas because each umbrella has its
own bare-token set documented next to its parser.

---

## Rule 4 — Token replacement uses the umbrella's most-specific name

When a value contains a placeholder token (uppercase, e.g.
`SESSION`), the parser replaces it with the most-specific
related value the umbrella knows about. For `--session`:

- `SESSION` token in `path` resolves to `session_name`
  (which is itself either explicitly set via `name:` or
  long-form, or auto-generated).

This lets one config string template per-run distinct values
without changing the config between invocations.

---

## Rule 5 — Unknown keys are warnings, not errors

Typoed or future-version keys log a `Warn` and are skipped.
The remaining valid items still apply. Operators see the
warning; they're not punished for a typo by losing the entire
spec.

(Long-form flags with unknown names follow normal CLI
unknown-flag handling — those are typically errors. The
forgiving rule applies only inside the umbrella's kv list.)

---

## Rule 6 — JSON is the long-form serialization, and it's unambiguous

The umbrella value is intentionally **not** valid JSON. A
canonical kv-list like `name:foo,path:bar` cannot be confused
with a JSON object because the comma-separated `key:value`
form lacks the surrounding `{...}` and quoted-string
structure JSON requires.

This means: **a value that parses as valid JSON is JSON, never
the umbrella's kv-list form.** A future option may accept JSON
at the same flag (e.g. `--session '{"name":"foo"}'`) without
ambiguity.

This rule applies more broadly to the codebase: **Polydat's
parameter-interpolation syntax `{...}` is, by design and by
definition, NOT valid JSON.** Any clause that parses
successfully as JSON is JSON, definitively not Polydat parameter
syntax. Parsers can use successful JSON parsing as the
disambiguation signal.

---

## Rule 7 — Every CLI flag has an `NMBRS_`-prefixed env var

Every config flag in nmbrs has an env-var equivalent. The env
name is derived from the flag name automatically:

```
--session            → NMBRS_SESSION
--session-name       → NMBRS_SESSION_NAME
--session-path       → NMBRS_SESSION_PATH
--session-reuse      → NMBRS_SESSION_REUSE
--session-keep       → NMBRS_SESSION_KEEP
--session-shelflife  → NMBRS_SESSION_SHELFLIFE
```

(Strip the leading `--`, replace `-` with `_`, uppercase,
prepend `NMBRS_`.)

**Setting both the CLI flag AND its `NMBRS_`-prefixed env var
is a hard error.** The runtime exits with status 2 and a
message naming both inputs. We refuse to silently
disambiguate — the operator's two inputs are fighting and one
of them is wrong.

This applies to every flag, umbrella and long-form alike.
There's no precedence rule for "which one wins" because
"both are set" is itself the error condition; pick one before
running.

The implementation lives in
`nmbrs_runtime::session::resolve_flag` and is reusable for
every flag site that adopts this pattern.

---

## Rule 8 — Documentation lives next to the parser

Every umbrella-options surface lists its keys, bare tokens,
and long-form-flag mappings in a single table next to its
parser implementation (rustdoc). The user-facing reference
groups them under one short name (e.g. "session options")
in the CLI help and the operator's guide.

---

## Test surface

The pilot surface in `--session` ships with parser tests
covering:

- Umbrella alone with single kv.
- Umbrella with multi-kv list and bare token.
- Long-form flag overrides umbrella.
- Env var falls back when neither umbrella nor long-form is
  present.
- Token replacement when the related key is set vs. when it's
  not (auto-id fallback).
- Unknown key emits warn but doesn't drop other items.

Future umbrella surfaces should mirror this test surface.

---

## See also

- SRD-45 — Sessions (the pilot adoption)
- SRD-10 §"String Interpolation" — Polydat `{...}` syntax that
  Rule 6 explicitly distinguishes from JSON
