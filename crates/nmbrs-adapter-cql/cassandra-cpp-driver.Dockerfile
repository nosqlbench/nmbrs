# Dockerfile for building the Apache Cassandra C++ driver as a static
# library. The base image matches the host OS to ensure glibc/ABI
# compatibility when linking the static lib into Rust on the host.

ARG BASE_IMAGE=ubuntu:22.04

FROM ${BASE_IMAGE} AS builder

# Pin to a release tag so the cpp-driver layer cache is stable
# across runs. Override via --build-arg for ad-hoc bumps.
ARG CASSANDRA_CPP_DRIVER_VERSION=2.17.1

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    cmake \
    git \
    libuv1-dev \
    libssl-dev \
    zlib1g-dev \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN git clone --depth 1 --branch ${CASSANDRA_CPP_DRIVER_VERSION} \
    https://github.com/apache/cassandra-cpp-driver.git /tmp/cass

RUN cd /tmp/cass \
    && mkdir build && cd build \
    && cmake .. \
        -DCMAKE_INSTALL_PREFIX=/usr/local \
        -DCMAKE_INSTALL_LIBDIR=lib \
        -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
        -DCMAKE_BUILD_TYPE=Release \
        -DCASS_BUILD_STATIC=ON \
        -DCASS_BUILD_SHARED=OFF \
        -DCMAKE_C_FLAGS="-fPIC" \
        -DCMAKE_CXX_FLAGS="-fPIC" \
    && make -j$(nproc) \
    && make install

# Verify
RUN find /usr/local -name "*cassandra*" -ls && ls /usr/local/include/cassandra.h

# --- Output: just the static lib + headers ---
FROM ${BASE_IMAGE}
COPY --from=builder /usr/local/lib/ /usr/local/lib/
COPY --from=builder /usr/local/include/cassandra.h /usr/local/include/
