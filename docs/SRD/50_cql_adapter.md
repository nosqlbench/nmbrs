# 50: CQL Adapter

The CQL adapter provides native Cassandra protocol access via the
Apache Cassandra C++ driver.

---

## Driver

`cassandra-cpp` Rust crate wrapping the Apache Cassandra C++
driver via FFI. Statically linked from a Docker-built sysroot.

Build: `cd adapters/cql && bash build.sh`

---

## Connection

```yaml
params:
  hosts: "127.0.0.1"
  port: "9042"
  keyspace: baselines
  consistency: LOCAL_ONE
  username: cassandra
  password: cassandra
  request_timeout_ms: "12000"
```

The adapter creates one `cass::Session`, shared across all
fibers via `Arc`. The C++ driver handles connection pooling
internally.

---

## Statement Modes

Op field names select the execution mode:

| Field Name | Mode | Use Case |
|------------|------|----------|
| `raw:` | String interpolation, direct execute | DDL, ad-hoc |
| `simple:` | Parameterized (future) | Simple queries |
| `prepared:` | Prepare once, bind per cycle | DML, queries |
| `stmt:` | Alias for prepared | Default |

### Mode Dispatch

```rust
const STMT_FIELD_NAMES: &[&str] = &["raw", "simple", "prepared", "stmt"];

// In map_op():
// 1. Find which field name is present
// 2. If "raw" or "simple" → CqlRawDispenser
// 3. If "prepared"/"stmt" with bind points → CqlPreparedDispenser
// 4. If "prepared"/"stmt" without bind points → CqlRawDispenser (DDL)
```

This dispatch is entirely adapter-specific. The core workload
model carries the field names as-is — no `stmt_type` in
`ParsedOp`.

---

## Dispensers

### CqlRawDispenser

Reads the fully-interpolated statement text from `ResolvedFields`
and executes it directly. All bind points are resolved to text
by the Polydat synthesis pipeline before the adapter sees them.

Used for: DDL (`CREATE TABLE`, `DROP INDEX`), simple queries.

### CqlPreparedDispenser

Prepares the statement lazily on first execute. Subsequent cycles
bind typed values by name:

```rust
// Bind typed values from ResolvedFields
for name in &self.bind_names {
    match fields.get_value(name) {
        Value::U64(v) => stmt.bind_int64_by_name(name, *v as i64),
        Value::F64(v) => stmt.bind_double_by_name(name, *v),
        Value::Bool(v) => stmt.bind_bool_by_name(name, *v),
        Value::Str(v) => stmt.bind_string_by_name(name, v),
        Value::Bytes(v) => stmt.bind_bytes_by_name(name, v.clone()),
        _ => stmt.bind_string_by_name(name, &value.to_display_string()),
    }
}
```

Bind point names are all op field keys except the statement field.

---

## CqlResultBody

Native result type carrying typed row data:

```rust
pub struct CqlResultBody {
    pub rows: Vec<HashMap<String, serde_json::Value>>,
}
```

Rows extracted from `CassResult` via `LendingIterator`, with
column values converted to JSON types. Supports:
- `to_json()` → JSON array of row objects
- `as_any()` → downcast for native access
- `element_count()` → row count
- `get_column_i64_values(name)` → integer column extraction
- `get_column_string_values(name)` → string column extraction

---

## Vector Workload

The CQL adapter supports ANN vector search workloads with
recall measurement:

```yaml
params:
  dataset: example
  k: "100"
  concurrency: "100"

bindings: |
  input cycle: u64
  train_vector := vector_at(cycle, "{dataset}")
  query_vector := query_vector_at(cycle, "{dataset}")
  ground_truth := neighbor_indices_at(cycle, "{dataset}")

ops:
  # Schema: raw mode for DDL
  create_table:
    raw: "CREATE TABLE t (key text PRIMARY KEY, value vector<float, {dim}>)"

  # Rampup: prepared mode for inserts
  insert:
    prepared: "INSERT INTO t (key, value) VALUES ('{id}', {train_vector})"

  # Search: raw mode (string-interpolated query) with recall
  select_ann:
    prepared: "SELECT key FROM t ORDER BY value ANN OF {query_vector} LIMIT {k}"
    relevancy:
      actual: key
      expected: "{ground_truth}"
      k: 100
      functions: [recall, precision, f1]
```

Verified: recall@100 = 0.94 on the 25-dim public reference set (1.2M vectors,
Cassandra 5.0 SAI).

---

## Error Names

| Error Name | Scope | Description |
|-----------|-------|-------------|
| `cql_error` | Op | Query execution failure |
| `prepare_error` | Op | Statement preparation failure |
| `bind_error` | Op | Value binding failure |
| `missing_field` | Op | Required field not in ResolvedFields |
