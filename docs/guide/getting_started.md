# Getting Started with nmbrs

nmbrs is a performance testing tool for databases and services. It
generates deterministic workloads from YAML definitions and dispatches
them through pluggable adapters.

---

## Quick Start

### 1. Build

```
cargo build --release
```

The binary is `target/release/nmbrs`.

### 2. Create a Workload

Create `my_workload.yaml`:

```yaml
bindings:
  user_id: Hash(); Mod(1000000)
  name: Identity()

ops:
  insert_user:
    stmt: "INSERT INTO users (id, name) VALUES ({user_id}, 'user_{name}');"
  read_user:
    stmt: "SELECT * FROM users WHERE id={user_id};"
```

### 3. Run It

```
nmbrs run workload=my_workload.yaml cycles=10
```

No workload yet? The binary carries a catalog (SRD-85):

```bash
nmbrs describe workloads          # curated workloads bundled in this binary
nmbrs run workload=selfcheck      # artifact self-check, no files needed
nmbrs copy capacity_probe         # materialize one locally for editing
```

Output:
```
nmbrs: 2 ops selected, 10 cycles, 1 threads, adapter=stdout
nmbrs: stanza length=2, sequencer=Bucket
INSERT INTO users (id, name) VALUES (527897, 'user_0');
SELECT * FROM users WHERE id=460078;
INSERT INTO users (id, name) VALUES (564547, 'user_2');
SELECT * FROM users WHERE id=960189;
...
nmbrs: done
```

Each `{user_id}` is replaced with a deterministic pseudo-random value
derived from the cycle number. Same cycle always produces the same
output.

---

## Workload Structure

A workload YAML has these sections:

```yaml
description: "What this workload does"

bindings:
  binding_name: Expression()

blocks:
  block_name:
    ops:
      op_name:
        ratio: 3
        stmt: "SQL or payload with {binding_name}"
```

### Bindings

Bindings generate data. Each binding is a function expression that
maps a cycle number to a value:

| Expression | What it produces |
|-----------|-----------------|
| `Identity()` | The cycle number itself (0, 1, 2, ...) |
| `Hash(); Mod(N)` | Pseudo-random value in [0, N) |
| `Hash(); Mod(1000000)` | Random ID up to 1M |

Bind points in op fields (`{binding_name}`) are replaced with the
binding's output for each cycle.

### Blocks and Tags

Blocks group ops by phase. Each block auto-tags its ops:

```yaml
blocks:
  schema:
    ops:
      create_table:
        stmt: "CREATE TABLE t (id int PRIMARY KEY);"
  main:
    ops:
      read:
        ratio: 5
        stmt: "SELECT * FROM t WHERE id={id};"
      write:
        ratio: 1
        stmt: "INSERT INTO t (id) VALUES ({id});"
```

Run a specific block with tag filtering:

```
nmbrs run workload=file.yaml tags=block:main cycles=1000
nmbrs run workload=file.yaml tags=block:schema cycles=1
```

### Ratios

Ops have a `ratio` (default 1) that controls how often they appear
in the sequence. With `read:5, write:1`, reads execute 5x more often
than writes.

---

## Command-Line Parameters

```
nmbrs run workload=<file> [parameters...]
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `workload=` | (required) | YAML workload file |
| `driver=` | `stdout` | Adapter: `stdout` |
| `cycles=` | `1` | Number of cycles (supports K, M, B suffixes) |
| `threads=` | `1` | Concurrency level |
| `rate=` | unlimited | Per-cycle rate limit (ops/sec) |
| `stanzarate=` | unlimited | Per-stanza rate limit |
| `tags=` | all ops | Tag filter (e.g., `block:main`) |
| `seq=` | `bucket` | Sequencer: `bucket`, `interval`, `concat` |
| `format=` | `stmt` | Output: `stmt`, `json`, `csv`, `assignments` |
| `errors=` | `.*:warn,counter` | Error handler spec |
| `filename=` | `stdout` | Output file (or `stdout`) |
| `watch=` | (off) | Phase-end re-render triggers — see [Watch Triggers](#watch-triggers) |

### Watch Triggers

`watch=<spec>[,<spec>...]` registers subprocess-based
re-renderers that fire after every phase completion or
failure. Each trigger spawns an `nmbrs` subprocess that
writes its output against the live session directory, so
external viewers (image viewer, browser, file watcher) see
the latest data without waiting for the run to finish.

| Spec | Subprocess invoked |
|------|--------------------|
| `report` | `nmbrs report all --session <SESSION>` |
| `report:<args>` | `nmbrs report <args> --session <SESSION>` |
| `plot` | `nmbrs report plot all --session <SESSION>` |
| `plot:<name>` | `nmbrs report plot --name <name> --session <SESSION>` |

Examples:

```bash
# Re-render the throughput plot after each phase:
nmbrs run workload=fknn.yaml watch=plot:throughput

# Stack triggers — both fire per phase end:
nmbrs run workload=fknn.yaml watch=plot:throughput,plot:recall

# Rebuild a full report on each phase end:
nmbrs run workload=fknn.yaml watch=report
```

Triggers run on a single background worker thread, sequenced
in registration order; a panic in one trigger doesn't stop
later ones. Subprocess `stdout` is suppressed; non-zero exit
surfaces as a parent-side WARN log. See
[CLI sysref §Watch Triggers](../sysref/60_cli.md#watch-triggers)
for the full reference.

### Cycle Count Suffixes

```
cycles=100       → 100
cycles=10K       → 10,000
cycles=1M        → 1,000,000
cycles=1B        → 1,000,000,000
```

---

## Output Formats

### Statement (default: `format=stmt`)

Shows just the `stmt` field:
```
INSERT INTO users (id) VALUES (527897);
SELECT * FROM users WHERE id=460078;
```

### JSON (`format=json`)

Full op as JSON:
```json
{"stmt":"INSERT INTO users (id) VALUES (527897);"}
```

### Assignments (`format=assignments`)

Key=value pairs:
```
stmt=INSERT INTO users (id) VALUES (527897);
```

### CSV (`format=csv`)

Comma-separated values:
```
INSERT INTO users (id) VALUES (527897);
```

---

## Sequencing Strategies

Control how ops are arranged within a stanza:

### Bucket (default)

Interleaved round-robin:
```
read:3, write:1 → read, write, read, read, read, write, read, read, ...
```

### Interval

Evenly spaced:
```
read:3, write:1 → read, read, write, read, read, read, write, read, ...
```

### Concat

Sequential blocks:
```
read:3, write:1 → read, read, read, write, read, read, read, write, ...
```

---

## Rate Limiting

Control throughput:

```
# 1000 ops per second
nmbrs run workload=w.yaml cycles=10K rate=1000

# 100 stanzas per second (each stanza = sum of ratios)
nmbrs run workload=w.yaml cycles=10K stanzarate=100
```

Both can be combined. The tighter limit wins.

---

## Error Handling

Configure how errors are handled:

```
# Stop on any error (default for production)
nmbrs run workload=w.yaml errors=".*:stop"

# Warn and count (default)
nmbrs run workload=w.yaml errors=".*:warn,counter"

# Retry timeouts, stop on everything else
nmbrs run workload=w.yaml errors="Timeout:retry,warn;.*:stop"
```

Handler modes: `stop`, `warn`, `error`, `ignore`, `retry`, `counter`.
Comma-separated modes execute in chain order.

---

## Template Variables

Parameterize workloads:

```yaml
bindings:
  id: Hash(); Mod(TEMPLATE(keycount, 1000000))
ops:
  read:
    stmt: "SELECT * FROM TEMPLATE(table, users) WHERE id={id};"
```

Override from CLI:

```
nmbrs run workload=w.yaml cycles=1K keycount=5000000 table=customers
```

---

## Example Workloads

### Key-Value Store

```yaml
bindings:
  key: Hash(); Mod(1000000)
  value: Hash(); Mod(999999)

blocks:
  main:
    ops:
      write:
        ratio: 1
        stmt: "SET key_{key} value_{value}"
      read:
        ratio: 5
        stmt: "GET key_{key}"
```

### Time-Series

```yaml
bindings:
  sensor_id: Hash(); Mod(1000)
  reading: Hash(); Mod(10000)
  timestamp: Identity()

ops:
  insert:
    stmt: "INSERT INTO readings (sensor, ts, value) VALUES ({sensor_id}, {timestamp}, {reading});"
```

### HTTP API (when HTTP adapter is available)

```yaml
bindings:
  user_id: Hash(); Mod(100000)

ops:
  create:
    method: POST
    url: "/api/users/{user_id}"
    body: "{\"name\": \"user_{user_id}\"}"
  get:
    method: GET
    url: "/api/users/{user_id}"
```
