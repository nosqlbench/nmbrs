# nmbrs-adapter-cql

CQL adapter for nmbrs. **Multi-engine**: choose one or both
underlying drivers via Cargo features.

| Feature | Engine | C++ toolchain | crates.io build |
|---------|--------|---------------|-----------------|
| `engine-scylla` (default) | [scylla 1.6](https://crates.io/crates/scylla) — pure Rust | No | Yes |
| `engine-cassandra-cpp` | [cassandra-cpp](https://crates.io/crates/cassandra-cpp) → Apache Cassandra C++ driver | Yes (libcassandra + libuv + openssl) | No (needs sysroot) |
| `all-engines` | both | Yes | No |

## User-facing adapter name: `cql`

The crate registers exactly one top-level adapter, `cql`. Engines
participate via `AliasResolverEntry`s and are selected at runtime:

```bash
# Default — picks the lowest-rank engine (cassandra-cpp if both
# are linked; scylla otherwise).
nmbrs run adapter=cql workload=...

# Force a specific engine.
nmbrs run adapter=cql cqldriver=scylla
nmbrs run adapter=cql cqldriver=cassandra-cpp
```

Engine names (`scylla`, `cassandra-cpp`) are *not* user-facing
adapters — `adapter=scylla` will fail. This keeps a single
diagnostic surface (`cql`) regardless of which engines a binary
linked.

## Crate layout

- `src/common/` — engine-agnostic surface, always compiled:
  config parsing, consistency enum, op-mode dispatch, the
  `cql_timeuuid` Polydat node, default status metrics, and the `cql`
  meta-adapter `AdapterRegistration`.
- `src/scylla/` — `engine-scylla` feature. Pure-Rust driver.
- `src/cassandra_cpp/` — `engine-cassandra-cpp` feature. C++
  driver via FFI; needs the local `sysroot/` (build it via
  `bash build.sh driver`).
- `workloads/` — CQL workload YAMLs (key-value, vector search,
  compaction tests).
- `build.sh`, `nmbrs.Dockerfile`, `cassandra-cpp-driver.Dockerfile`,
  `sysroot/`, `.cargo/config.toml` — the build infrastructure for
  the cassandra-cpp engine. The default scylla build needs none
  of this.

## Building with the cassandra-cpp engine

```bash
cd adapters/cql
bash build.sh           # builds the C driver in Docker, then nmbrs
bash build.sh driver    # only the C driver → sysroot/
bash build.sh cargo     # only nmbrs (sysroot/ must exist)
bash build.sh docker    # everything inside Docker
```
