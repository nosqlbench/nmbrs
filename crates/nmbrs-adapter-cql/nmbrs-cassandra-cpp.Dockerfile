# Dockerfile for building nmbrs with the cassandra-cpp engine.
#
# Multi-stage build with cargo-chef for incremental rust dep
# caching:
#
#   cpp-driver  — builds the Apache Cassandra C++ driver static
#                 archive + headers. Cached by docker layer cache
#                 as long as the pinned ref doesn't change.
#   rust-base   — Rust toolchain, system build deps, cargo-chef.
#                 Cached as long as apt set + cargo-chef version
#                 don't change.
#   planner     — runs `cargo chef prepare`. Produces a recipe
#                 derived only from Cargo.toml / Cargo.lock; a
#                 code-only edit produces an identical recipe.
#   deps        — runs `cargo chef cook`. Compiles all third-party
#                 crate deps from the recipe. Cache-hits as long
#                 as the recipe hasn't changed — code edits don't
#                 invalidate this layer.
#   rust-builder— `COPY . .` + `cargo build`. Reuses the deps
#                 target/ from the deps stage; only workspace
#                 crates recompile.
#   runtime     — slim ubuntu:24.04 with libuv1 + libssl3t64 +
#                 the cpp-driver shared libs. ENTRYPOINT is nmbrs.
#
# Build context is the workspace root:
#   docker build -f crates/nmbrs-adapter-cql/nmbrs-cassandra-cpp.Dockerfile -t nmbrs-cassandra-cpp .
#   docker run --rm nmbrs-cassandra-cpp --help
#
# To connect to a Cassandra cluster:
#   docker run --rm --network host nmbrs-cassandra-cpp run \
#     adapter=cql cqldriver=cassandra-cpp \
#     hosts=localhost keyspace=myks ...

# ─── Stage 1: Apache Cassandra C++ driver ─────────────────────
FROM ubuntu:24.04 AS cpp-driver

# Pinned driver ref. Override via --build-arg for ad-hoc bumps.
# Layer cache is stable across runs at this pin.
ARG CASSANDRA_CPP_DRIVER_VERSION=2.17.1

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential cmake git libuv1-dev libssl-dev zlib1g-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# CMAKE_INSTALL_LIBDIR=lib forces a flat lib/ install regardless
# of multiarch detection — without it, /usr/local triggers
# `lib/<arch-triple>/` and upstream cassandra-cpp-sys's link
# search list only covers x86_64-linux-gnu.
RUN git clone --depth 1 --branch ${CASSANDRA_CPP_DRIVER_VERSION} \
        https://github.com/apache/cassandra-cpp-driver.git /tmp/cass \
    && cd /tmp/cass && mkdir build && cd build \
    && cmake .. \
        -DCMAKE_BUILD_TYPE=Release \
        -DCASS_BUILD_STATIC=ON \
        -DCMAKE_INSTALL_LIBDIR=lib \
        -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
    && make -j$(nproc) && make install && ldconfig

# ─── Stage 2: rust-base — toolchain, apt deps, cargo-chef ─────
FROM ubuntu:24.04 AS rust-base

# Pin cargo-chef so the rust-base layer is reproducible across
# runs. Bump in lockstep with toolchain refreshes.
ARG CARGO_CHEF_VERSION=0.1.71

# clang + mold required by the workspace .cargo/config.toml
# `linker = "clang"` / `link-arg=-fuse-ld=mold` / `link-arg=-lz`.
RUN apt-get update && apt-get install -y --no-install-recommends \
        curl build-essential pkg-config libuv1-dev libssl-dev zlib1g-dev \
        clang mold ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

ENV PATH="/root/.cargo/bin:${PATH}"

RUN cargo install cargo-chef --locked --version ${CARGO_CHEF_VERSION}

# Pull in the cpp-driver libs + header. Multi-stage COPY happens
# regardless of cargo-chef state, so the deps stage can link
# against libcassandra during `cargo chef cook` if any dep's
# build.rs probes for it (currently none do, but defending is
# free — the COPY is a stable layer that cache-hits as long as
# the cpp-driver pin doesn't change).
COPY --from=cpp-driver /usr/local/lib/ /usr/local/lib/
COPY --from=cpp-driver /usr/local/include/cassandra.h /usr/local/include/
RUN ldconfig

WORKDIR /src

# ─── Stage 3: planner — recipe.json from current source tree ──
FROM rust-base AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ─── Stage 4: deps — compile all third-party crates from recipe.
# A code-only edit produces the same recipe.json hash → this
# layer cache-hits. Only Cargo.toml / Cargo.lock changes
# invalidate it.
FROM rust-base AS deps
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json \
        -p nmbrs --no-default-features --features engine-cassandra-cpp

# ─── Stage 5: rust-builder — compile workspace src using cached
# deps target/. Reuses /src/target from the deps stage.
FROM deps AS rust-builder
COPY . .
RUN cargo build --release -p nmbrs \
        --no-default-features --features engine-cassandra-cpp

# ─── Stage 6: Runtime ─────────────────────────────────────────
FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
        libuv1 libssl3t64 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=cpp-driver /usr/local/lib/ /usr/local/lib/
RUN ldconfig

COPY --from=rust-builder /src/target/release/nmbrs /usr/local/bin/nmbrs

ENTRYPOINT ["nmbrs"]
