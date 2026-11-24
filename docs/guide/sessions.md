# Sessions — operator's guide

Every `nmbrs run` is a *session*. The session owns a directory
on disk where every artifact from the run lands —
`metrics.db`, `session.log`, `checkpoint.json`, `summary.md`,
flame graphs, TUI dumps. Other commands (`nmbrs report`,
`nmbrs replay`, `nmbrs metrics`) read from a session.

This guide walks through the everyday patterns. The
authoritative spec is **SRD-45**.

---

## The simplest case

Out of the box, `nmbrs run workload=foo.yaml` writes to
`logs/<scenario>_<timestamp>/`. The shell-friendly
`logs/latest` symlink always points at the most recent
session, so `nmbrs report plot` / `nmbrs report` work without
arguments:

```
$ nmbrs run workload=foo.yaml
$ nmbrs report plot      # reads logs/latest/metrics.db
$ nmbrs report    # ditto
```

That's it. Most users never need anything else.

---

## Pinning a session name

Sometimes you want a memorable name instead of the
auto-generated timestamp:

```
$ nmbrs run workload=foo.yaml --session=baseline
# writes to logs/baseline/

$ nmbrs run workload=foo.yaml --session=tuning1
# writes to logs/tuning1/
```

Then `nmbrs report plot --db logs/baseline/metrics.db` (or
`NMBRS_SESSION_PATH=logs/baseline nmbrs report plot`) targets that
specific session even after you've started newer ones.

---

## Pinning a session directory

For longer runs / shared filesystems, point at an explicit
directory:

```
$ nmbrs run workload=foo.yaml --session-path=/data/runs/2026-q1-baseline
```

The basename (`2026-q1-baseline`) becomes the session id.
`logs/latest` is updated to point absolute-path at the new
location, so `nmbrs report plot` still finds it.

### Templating with the `SESSION` token

If you want one shell-level pin that produces distinct
per-run directories, put the literal token `SESSION` in the
path:

```
$ export NMBRS_SESSION_PATH=/data/runs/SESSION_dir
$ nmbrs run workload=foo.yaml
# writes to /data/runs/default_20260101_120000_dir/

$ nmbrs run workload=foo.yaml
# writes to /data/runs/default_20260101_120100_dir/
```

The token is replaced with the auto-generated session name at
write time. `--session-path` and `NMBRS_SESSION_PATH` are
equivalent; the env var is just shorthand so a shell can
share it with sibling subcommands.

---

## NMBRS_SESSION_PATH in a subshell

Set the env var once, every subcommand in that subshell sees
the same session directory:

```
$ export NMBRS_SESSION_PATH=/data/runs/today
$ nmbrs run workload=foo.yaml
$ nmbrs report plot                          # reads /data/runs/today/metrics.db
$ nmbrs report                        # ditto
$ nmbrs run workload=foo.yaml --resume   # resumes /data/runs/today
```

This is the recommended pattern when iterating on a workload —
no need to copy paths around between commands.

---

## Reusing an existing session directory

If a session directory **already contains artifacts** (a prior
run's `metrics.db`, `session.log`, or `checkpoint.json`),
`nmbrs run` refuses to start by default — it won't silently
destroy the prior session. You have three options, picked via
`--session-reuse`:

| Mode | What happens |
| --- | --- |
| `error` (default) | Run aborts with exit code 2 and a message naming the existing path. |
| `restart` | Prior artifacts deleted, fresh run starts in the same dir. |
| `resume` | Refuses to start unless you also pass `--resume`. Reminds you that resume continues the prior session rather than overwriting. |

Examples:

```
# Wipe and restart in the same dir
$ nmbrs run workload=foo.yaml --session=baseline --session-reuse=restart

# Continue the prior session (idempotent phases skip; failed ones rerun)
$ nmbrs run workload=foo.yaml --session=baseline --resume
```

If you really do want destructive overwrite without thinking,
make it explicit. The default protects against typos.

---

## Resume after failure

When a workload's idempotent phases fail or stop short, the
runtime tells you on exit:

```
This session has resumable phases that didn't complete.
  To continue from where it stopped:
    nmbrs run <workload> --session-path /data/runs/today --resume
  To pin the session name for repeatable resumes:
    nmbrs run <workload> --session today (then add --resume next time)
```

The hint fires automatically — no action needed beyond reading
the message. See SRD-44 for the resume model itself.

---

## Automatic cleanup

By default, the runtime keeps the **10 most recent** sessions
under each session-parent directory. Older sessions are purged
at the start of the next command that CREATES a session — a
read-only command never deletes one. Tunable:

```
$ nmbrs run workload=foo.yaml --session-keep=5     # keep just 5
$ nmbrs run workload=foo.yaml --session-keep=0     # never purge by count
```

Sessions older than **4 weeks** are also purged regardless of
the count cap. Tunable:

```
$ nmbrs run workload=foo.yaml --session-shelflife=2w  # keep 2 weeks
$ nmbrs run workload=foo.yaml --session-shelflife=0   # never purge by age
```

Duration syntax: `<n>s|m|h|d|w` (seconds / minutes / hours /
days / weeks). Bare integers are seconds.

Env-var equivalents: `NMBRS_SESSION_KEEP`, `NMBRS_SESSION_SHELFLIFE`.

The active session (`logs/latest` and its target) is never
purged regardless of policy. Symlinks at the parent level
are skipped.

---

## CLI quick reference

Env-var names are derived from the flag: `--foo-bar` → `NMBRS_FOO_BAR`.

| Flag | Env | Default | Use it when… |
| --- | --- | --- | --- |
| `--session <name\|k:v,…>` | `NMBRS_SESSION` | auto | You want a memorable session id, or want to set several session settings at once (`--session=name:x,keep:5`). |
| `--session-name <name>` | `NMBRS_SESSION_NAME` | auto | Only the id needs overriding. |
| `--session-path <path>` | `NMBRS_SESSION_PATH` | unset | You want full control over the path; use the `SESSION` token for per-run templating. |
| `--session-reuse <mode>` | `NMBRS_SESSION_REUSE` | `error` | A session dir already exists and you've decided what to do with it (`error` / `restart` / `resume`). |
| `--session-keep <N>` | `NMBRS_SESSION_KEEP` | `10` | Custom retention count (`0` disables). |
| `--session-shelflife <dur>` | `NMBRS_SESSION_SHELFLIFE` | `4w` | Custom retention age (`0` disables). |
| `--resume[ <id>]` | `NMBRS_RESUME` | unset | Continue a prior session from where it stopped. |
| `--resume-latest` | — | unset | Continue from `sessions/latest`. |

The env vars are shorthand for the equivalent flag — useful in
shell sessions that want consistent wiring across multiple
`nmbrs` invocations. Setting both the flag and its env var is a
configuration conflict and exits rather than silently picking one.

There is no `--logs-dir`. To put sessions under a custom parent,
give the parent in the path: `--session-path=/data/runs/SESSION`.

---

## See also

- SRD-45 — Sessions (authoritative)
- SRD-44 — Workload checkpointing (resume / skip / verify)
- `docs/guide/polydat_purity.md` — when to mark wires `volatile`
  to keep their values out of resume identity
