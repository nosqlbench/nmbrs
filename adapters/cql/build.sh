#!/bin/bash
# Build script for the cassandra-cpp engine of nmbrs-adapter-cql.
#
# This script:
#   1. Builds the Apache Cassandra C++ driver from source in
#      Docker (matching the host OS for ABI compatibility).
#   2. Extracts the static library and headers into the cql
#      adapter's per-crate target directory:
#      adapters/cql/target/sysroot/.
#   3. Builds nmbrs with --features engine-cassandra-cpp,
#      linking statically against the driver.
#
# Lifecycle:
#   Every artifact this script produces lives under
#   `adapters/cql/target/`, which cargo manages via the
#   `[build] target-dir = "target"` setting in
#   `adapters/cql/.cargo/config.toml`. The script itself
#   never runs `rm`: fresh state comes from docker-cp
#   overwriting files of the same name, and cleanup is
#   `cargo clean` (run from adapters/cql/).
#
# Usage:
#   cd adapters/cql
#   bash build.sh           # full build (driver + nmbrs)
#   bash build.sh driver    # build only the C++ driver
#   bash build.sh cargo     # build only nmbrs (driver must exist)
#   bash build.sh install   # cargo install --path nmbrs with all engines linked
#   bash build.sh docker    # build nmbrs entirely inside Docker
#   bash build.sh clean     # `cargo clean` (this crate) + docker rmi
#
# Driver build mode (selects how the C++ driver is compiled):
#   DRIVER_BUILD_MODE=docker  (default) — build in Docker, no host
#                              package pollution. Right for local dev.
#   DRIVER_BUILD_MODE=native            — build directly on the host.
#                              Caller is responsible for apt deps
#                              (build-essential cmake git libuv1-dev
#                              libssl-dev zlib1g-dev). Used by CI
#                              runners whose OS == target ABI.
# Both modes produce an identical `target/sysroot/` layout; the
# `cargo`, `install`, and `docker` subcommands don't care which
# one was used. The CMake recipe + sysroot post-processing live
# in shared functions so divergence is impossible.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Pre-cargo linker artifacts (C++ driver static lib + header)
# and the docker-extracted nmbrs binary all live under the
# adapter-local cargo target/ — see adapters/cql/.cargo/config.toml
# (`[build] target-dir = "target"`). That makes them part of
# cargo's lifecycle: `cd adapters/cql && cargo clean` wipes
# everything; `cargo clean` at workspace root leaves them
# alone (different target dir entirely).
#
# Build script never runs `rm` — fresh-state mechanics defer
# either to docker (overwriting copies) or to cargo clean.
ADAPTER_TARGET="$SCRIPT_DIR/target"
SYSROOT="$ADAPTER_TARGET/sysroot"
DOCKER_NMBRS="$ADAPTER_TARGET/nmbrs"
DOCKER_CONTEXT="$ADAPTER_TARGET/docker-context"
DOCKER_IMAGE="nmbrs-cql-cpp-driver-builder"

# ─── Build the C++ driver — dispatches on DRIVER_BUILD_MODE ───
#
# Both modes populate $SYSROOT/{lib,include} with the same files
# (the CMake recipe lives in cassandra-cpp-driver.Dockerfile and
# in `_build_driver_native` and is identical across the two —
# the only difference is where the compile happens). Both share
# `_finalize_sysroot` for the multiarch flatten and the
# libcassandra.a alias, so a downstream consumer cannot tell
# which mode produced the sysroot.

build_driver() {
    mkdir -p "$SYSROOT/lib" "$SYSROOT/include"
    local mode="${DRIVER_BUILD_MODE:-docker}"
    case "$mode" in
        docker) _build_driver_docker ;;
        native) _build_driver_native ;;
        *)
            echo "ERROR: unknown DRIVER_BUILD_MODE: $mode (expected docker|native)" >&2
            exit 1
            ;;
    esac
    _finalize_sysroot
}

_build_driver_docker() {
    # Detect host OS for matching Docker base image
    local base_image="ubuntu:22.04"
    if [ -f /etc/os-release ]; then
        local version_id
        version_id=$(grep '^VERSION_ID=' /etc/os-release | cut -d'"' -f2)
        if [ -n "$version_id" ]; then
            base_image="ubuntu:${version_id}"
        fi
    fi
    echo "==> [docker] Host OS: $(grep PRETTY_NAME /etc/os-release 2>/dev/null | cut -d'"' -f2)"
    echo "==> [docker] Docker base: $base_image"
    echo "==> [docker] Building Apache Cassandra C++ driver..."
    docker build \
        --build-arg BASE_IMAGE="$base_image" \
        -f "$SCRIPT_DIR/cassandra-cpp-driver.Dockerfile" \
        -t "$DOCKER_IMAGE" \
        "$SCRIPT_DIR"

    echo "==> [docker] Extracting libraries and headers to $SYSROOT..."
    # Fresh-state mechanics:
    #   - The Dockerfile builds with `CASS_BUILD_SHARED=OFF` so only
    #     `libcassandra_static.a` lands in `/usr/local/lib/` — no
    #     `.so*` to clean up.
    #   - `docker cp` overwrites existing files of the same name.
    # The user runs `cd adapters/cql && cargo clean` for fresh start.
    local cid
    cid=$(docker create "$DOCKER_IMAGE")
    docker cp "$cid:/usr/local/lib/." "$SYSROOT/lib/"
    docker cp "$cid:/usr/local/include/cassandra.h" "$SYSROOT/include/"
    docker rm "$cid" > /dev/null
}

_build_driver_native() {
    # Native build: clone + cmake + make on the host. Caller must
    # have already installed build deps. Used by CI runners where
    # host OS == target ABI environment, so Docker adds only
    # overhead. Staging dir lives under $ADAPTER_TARGET so
    # `cd adapters/cql && cargo clean` wipes it.
    local ref="${CASSANDRA_CPP_DRIVER_VERSION:-2.17.1}"
    local stage="$ADAPTER_TARGET/native-driver"
    local src="$stage/src"
    local prefix="$stage/prefix"
    local uname_s
    uname_s="$(uname -s)"

    # Platform-specific cmake hints + parallelism.
    local cmake_extra=()
    local jobs
    case "$uname_s" in
        Linux)
            jobs="$(nproc)"
            ;;
        Darwin)
            # brew's openssl@3 is keg-only; cmake's find_package
            # needs OPENSSL_ROOT_DIR to pick it up over the
            # deprecated /usr/lib/libssl.dylib.
            jobs="$(sysctl -n hw.ncpu)"
            if command -v brew >/dev/null 2>&1; then
                local brew_openssl
                brew_openssl="$(brew --prefix openssl@3 2>/dev/null || true)"
                if [ -n "$brew_openssl" ]; then
                    cmake_extra+=(-DOPENSSL_ROOT_DIR="$brew_openssl")
                    export PKG_CONFIG_PATH="$brew_openssl/lib/pkgconfig:${PKG_CONFIG_PATH:-}"
                fi
                # libuv is brewable as a normal formula — cmake's
                # find_library handles it via standard /opt/homebrew
                # or /usr/local search paths, so no explicit hint.
            fi
            ;;
        *)
            echo "ERROR: DRIVER_BUILD_MODE=native does not yet support $uname_s" >&2
            exit 1
            ;;
    esac

    # Pre-flight dep check — friendly error rather than a deep
    # cmake/make failure if the runner forgot to install deps.
    local missing=()
    command -v cmake >/dev/null 2>&1 || missing+=("cmake")
    command -v git   >/dev/null 2>&1 || missing+=("git")
    command -v make  >/dev/null 2>&1 || missing+=("make")
    if command -v pkg-config >/dev/null 2>&1; then
        pkg-config --exists libuv    2>/dev/null || missing+=("libuv")
        pkg-config --exists openssl  2>/dev/null || missing+=("openssl")
    else
        missing+=("pkg-config")
    fi
    if [ ${#missing[@]} -ne 0 ]; then
        echo "ERROR: DRIVER_BUILD_MODE=native needs: ${missing[*]}" >&2
        case "$uname_s" in
            Linux)
                echo "  ubuntu/debian: sudo apt-get install -y \\" >&2
                echo "    build-essential cmake git libuv1-dev libssl-dev zlib1g-dev pkg-config" >&2
                ;;
            Darwin)
                echo "  macOS (brew): brew install cmake libuv openssl@3 pkg-config" >&2
                ;;
        esac
        exit 1
    fi

    echo "==> [native] uname -s: $uname_s"
    echo "==> [native] cassandra-cpp-driver @ $ref"
    [ ${#cmake_extra[@]} -gt 0 ] && echo "==> [native] cmake hints: ${cmake_extra[*]}"

    mkdir -p "$stage" "$prefix"
    if [ ! -d "$src/.git" ]; then
        git clone --depth 1 --branch "$ref" \
            https://github.com/apache/cassandra-cpp-driver.git "$src"
    else
        echo "==> [native] reusing existing source clone at $src"
    fi

    # Upstream CMakeLists.txt uses `STREQUAL "Clang"` for compiler
    # detection, which excludes AppleClang and aborts with
    # "Unsupported compiler: AppleClang". AppleClang has been
    # binary-compatible with LLVM Clang for years; relaxing the
    # comparison to a regex match accepts both. Idempotent — sed
    # is a no-op once the patch has already been applied.
    if [ "$uname_s" = "Darwin" ]; then
        sed -i.bak 's/STREQUAL "Clang"/MATCHES "Clang"/g' "$src/CMakeLists.txt"
    fi

    echo "==> [native] cmake + make -j$jobs ..."
    mkdir -p "$src/build"
    (
        cd "$src/build"
        # CMAKE_INSTALL_LIBDIR=lib forces a flat lib/ install
        # regardless of multiarch detection. Otherwise cmake's
        # GNUInstallDirs may pick lib/<arch-triple>/ for prefixes
        # like /usr/local, and the rustc linker search paths
        # don't follow the multiarch convention.
        #
        # CMAKE_POLICY_VERSION_MINIMUM=3.5 keeps cassandra-cpp-driver
        # compatible with cmake 4.x — upstream's CMakeLists.txt
        # declares an older `cmake_minimum_required` that cmake 4
        # rejects outright. Harmless on cmake 3.x.
        cmake .. \
            -DCMAKE_INSTALL_PREFIX="$prefix" \
            -DCMAKE_INSTALL_LIBDIR=lib \
            -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
            -DCMAKE_BUILD_TYPE=Release \
            -DCASS_BUILD_STATIC=ON \
            -DCASS_BUILD_SHARED=OFF \
            -DCMAKE_C_FLAGS="-fPIC" \
            -DCMAKE_CXX_FLAGS="-fPIC" \
            "${cmake_extra[@]}"
        make -j"$jobs"
        make install
    )

    echo "==> [native] Copying libraries and headers to $SYSROOT..."
    cp -a "$prefix/lib/." "$SYSROOT/lib/"
    cp    "$prefix/include/cassandra.h" "$SYSROOT/include/"
}

_finalize_sysroot() {
    # Shared post-processing: identical regardless of build mode.
    # Flatten any multiarch subdirs (e.g. x86_64-linux-gnu,
    # aarch64-linux-gnu) into lib/. The cmake recipes set
    # CMAKE_INSTALL_LIBDIR=lib to avoid this, but keep the
    # flatten as defense in depth against future drift.
    for sub in "$SYSROOT"/lib/*-linux-gnu; do
        [ -d "$sub" ] || continue
        cp -a "$sub"/* "$SYSROOT/lib/"
    done

    # Create libcassandra.a symlink if only _static.a exists
    # (the -sys crate links -lcassandra, not -lcassandra_static).
    if [ -f "$SYSROOT/lib/libcassandra_static.a" ] && [ ! -f "$SYSROOT/lib/libcassandra.a" ]; then
        ln -sf libcassandra_static.a "$SYSROOT/lib/libcassandra.a"
    fi

    echo "==> Driver libraries:"
    ls -la "$SYSROOT/lib"/libcassandra* 2>/dev/null || echo "  (not found)"
    echo "==> Headers:"
    ls -la "$SYSROOT/include"/cassandra.h
    echo "==> Driver build complete."
}

# ─── Build nmbrs with cargo, using the local sysroot ───

build_cargo() {
    # The driver build (build_driver) deletes all `.so*` files
    # under sysroot — only the static archive remains. So check
    # for either the canonical name or the upstream-installed
    # `_static` variant.
    if [ ! -f "$SYSROOT/lib/libcassandra_static.a" ] \
       && [ ! -f "$SYSROOT/lib/libcassandra.a" ]; then
        echo "ERROR: Driver not found in $SYSROOT" >&2
        echo "  Run 'bash build.sh driver' first, or 'bash build.sh' for a full build." >&2
        exit 1
    fi

    echo "==> Building nmbrs --features engine-cassandra-cpp (static linking)..."

    # Point cassandra-cpp-sys at our sysroot
    export CASSANDRA_SYS_LIB_PATH="$SYSROOT/lib"
    export LIBRARY_PATH="$SYSROOT/lib:${LIBRARY_PATH:-}"
    export C_INCLUDE_PATH="$SYSROOT/include:${C_INCLUDE_PATH:-}"

    cd "$PROJECT_ROOT"

    cargo build --release -p nmbrs --no-default-features --features engine-cassandra-cpp

    local bin="$PROJECT_ROOT/target/release/nmbrs"

    if [ -f "$bin" ]; then
        echo "==> Built: $bin"
        # Verify it's statically linked against libcassandra
        if ldd "$bin" 2>/dev/null | grep -q "libcassandra"; then
            echo "  WARNING: dynamically linked to libcassandra (static link may have failed)"
        else
            echo "  libcassandra: statically linked"
        fi
        echo "==> To run: $bin --help"
    else
        echo "==> Build completed but binary not found."
    fi
}

# ─── cargo install --path nmbrs with the cassandra-cpp engine ───

build_install() {
    if [ ! -f "$SYSROOT/lib/libcassandra_static.a" ] \
       && [ ! -f "$SYSROOT/lib/libcassandra.a" ]; then
        echo "ERROR: Driver not found in $SYSROOT" >&2
        echo "  Run 'bash build.sh driver' first, or 'bash build.sh' for a full build." >&2
        exit 1
    fi

    echo "==> cargo install --path nmbrs --features all-engines..."

    # Same sysroot env as build_cargo so the linker picks up the
    # static libcassandra and the cassandra-cpp-sys crate's headers.
    export CASSANDRA_SYS_LIB_PATH="$SYSROOT/lib"
    export LIBRARY_PATH="$SYSROOT/lib:${LIBRARY_PATH:-}"
    export C_INCLUDE_PATH="$SYSROOT/include:${C_INCLUDE_PATH:-}"

    cd "$PROJECT_ROOT"

    # `--path nmbrs` from the workspace root. `--locked` keeps the
    # install hermetic against drift in Cargo.lock. `--force` lets
    # you re-run after iterating without removing the prior install.
    # `all-engines` links both scylla and cassandra-cpp so the
    # built binary can pick at runtime via `cqldriver=…`.
    cargo install --locked --force \
        --path nmbrs \
        --no-default-features --features all-engines

    local cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin/nmbrs"
    if [ -x "$cargo_bin" ]; then
        echo "==> Installed: $cargo_bin"
    else
        echo "==> Install completed but $cargo_bin not found (see cargo output above)"
    fi
}

# ─── Build everything inside Docker (no host Rust needed) ───

build_docker() {
    echo "==> Staging docker build context at $DOCKER_CONTEXT..."
    # Stage the workspace into a known dir so the build context
    # holds only what the image needs; target/ is huge and
    # excluded, and every dependency outside the workspace comes
    # from crates.io.
    #
    # The staging dir lives under our cargo-managed target/
    # so its lifecycle defers to `cargo clean`. `rsync --delete`
    # ensures fresh content each build without any rm step in
    # this script.
    mkdir -p "$DOCKER_CONTEXT"

    rsync -a --delete \
        --exclude=target \
        --exclude=.git \
        --exclude=links \
        "$PROJECT_ROOT/" "$DOCKER_CONTEXT/"

    echo "==> Context size: $(du -sh "$DOCKER_CONTEXT" | cut -f1)"
    echo "==> Building nmbrs (cassandra-cpp) entirely in Docker..."
    docker build \
        -f "$SCRIPT_DIR/nmbrs-cassandra-cpp.Dockerfile" \
        -t nmbrs-cassandra-cpp \
        "$DOCKER_CONTEXT"

    echo "==> Docker image: nmbrs-cassandra-cpp"
    echo "==> Run: docker run --rm --network host nmbrs-cassandra-cpp --help"

    # Optionally extract the binary
    echo "==> Extracting binary..."
    local cid
    cid=$(docker create nmbrs-cassandra-cpp)
    # Extract under the cargo-managed target/ alongside the
    # sysroot — `cargo clean` from this crate cleans it up.
    docker cp "$cid:/usr/local/bin/nmbrs" "$DOCKER_NMBRS"
    docker rm "$cid" > /dev/null
    echo "==> Extracted: $DOCKER_NMBRS"
}

# ─── Clean ───

clean() {
    echo "==> Cleaning..."

    # All file-system artifacts (sysroot, docker-extracted
    # nmbrs, docker context) live under `adapters/cql/target/`,
    # which is the per-crate target directory configured in
    # `adapters/cql/.cargo/config.toml`. `cargo clean` from
    # this crate dir owns its lifecycle — no rm in this
    # script.
    if ! command -v cargo >/dev/null 2>&1; then
        echo "ERROR: cargo not on PATH — cannot clean" >&2
        exit 1
    fi
    echo "==> cd $SCRIPT_DIR && cargo clean"
    (cd "$SCRIPT_DIR" && cargo clean)

    # Docker images — defer to docker's own rm.
    if command -v docker >/dev/null 2>&1; then
        for img in "$DOCKER_IMAGE" nmbrs-cassandra-cpp; do
            if docker image inspect "$img" >/dev/null 2>&1; then
                echo "==> docker rmi $img"
                docker rmi "$img" || true
            fi
        done
    else
        echo "WARN: docker not on PATH — driver/app images not cleaned" >&2
    fi

    echo "==> Clean complete."
}

# ─── Main ───


case "${1:-default}" in
    default)
        build_driver
        build_cargo
        ;;
    driver)
        build_driver
        ;;
    cargo)
        build_cargo
        ;;
    install)
        build_install
        ;;
    docker)
        build_docker
        ;;
    clean)
        clean
        ;;
    *)
        echo "Usage: bash build.sh [driver|cargo|install|docker|clean]"
        echo ""
        echo "  (default)    Build C++ driver, extract libs, cargo build on host"
        echo "  driver       Build only the C++ driver, extract to target/sysroot/"
        echo "  cargo        Build only nmbrs --features engine-cassandra-cpp (driver must exist)"
        echo "  install      cargo install --path nmbrs --features all-engines (driver must exist)"
        echo "  docker       Build everything inside Docker (no host Rust needed)"
        echo "  clean        cargo clean (this crate's target/) + docker rmi"
        echo ""
        echo "Env vars:"
        echo "  DRIVER_BUILD_MODE=docker (default) | native"
        echo "  CASSANDRA_CPP_DRIVER_VERSION=<git ref> (default: 2.17.1; native mode only)"
        exit 1
        ;;
esac
