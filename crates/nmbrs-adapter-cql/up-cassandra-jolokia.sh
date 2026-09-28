#!/usr/bin/env bash
# Swap the local `cassandra-test` Docker container for the
# Jolokia-enabled variant built from cassandra-jolokia.Dockerfile.
# Exposes:
#   9042  — CQL (same as the stock container)
#   8778  — Jolokia JMX/HTTP bridge
#
# Existing container's anonymous data volume is NOT preserved —
# benchmark scenarios re-rampup data anyway. Pass --keep-data to
# attempt a volume-name carry-over if you want the prior keyspace
# state to survive (best-effort; volume must still be mountable).
#
# Usage:
#   bash crates/nmbrs-adapter-cql/up-cassandra-jolokia.sh
#   bash crates/nmbrs-adapter-cql/up-cassandra-jolokia.sh --keep-data
#   bash crates/nmbrs-adapter-cql/up-cassandra-jolokia.sh --rebuild     # force docker build
#
# After it's up, sanity-check Jolokia:
#   curl -s http://localhost:8778/jolokia/version | jq .

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMAGE_TAG="${CASSANDRA_JOLOKIA_TAG:-cassandra-jolokia:latest}"
CONTAINER_NAME="${CASSANDRA_CONTAINER_NAME:-cassandra-test}"
KEEP_DATA=0
FORCE_REBUILD=0

for arg in "$@"; do
    case "$arg" in
        --keep-data)  KEEP_DATA=1 ;;
        --rebuild)    FORCE_REBUILD=1 ;;
        -h|--help)
            grep '^#' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *)
            echo "unknown arg: $arg" >&2
            exit 2 ;;
    esac
done

# Capture the prior data volume name BEFORE we tear down the
# container, in case --keep-data is set.
PRIOR_VOLUME=""
if [ "$KEEP_DATA" = "1" ] && docker inspect "$CONTAINER_NAME" >/dev/null 2>&1; then
    PRIOR_VOLUME=$(docker inspect "$CONTAINER_NAME" \
        --format '{{range .Mounts}}{{if eq .Destination "/var/lib/cassandra"}}{{.Name}}{{end}}{{end}}' \
        2>/dev/null || true)
    if [ -n "$PRIOR_VOLUME" ]; then
        echo "==> Preserving prior data volume: $PRIOR_VOLUME"
    fi
fi

# Build the image. `--rebuild` adds `--no-cache` so a changed
# `JOLOKIA_URL` / `JOLOKIA_VERSION` ARG actually re-downloads the
# jar (Docker's layer cache does NOT auto-invalidate on ARG
# changes consumed inside RUN — only on changes to the RUN line
# itself).
if [ "$FORCE_REBUILD" = "1" ] || ! docker image inspect "$IMAGE_TAG" >/dev/null 2>&1; then
    echo "==> Building $IMAGE_TAG"
    BUILD_FLAGS=()
    if [ "$FORCE_REBUILD" = "1" ]; then
        BUILD_FLAGS+=(--no-cache)
    fi
    docker build "${BUILD_FLAGS[@]}" -t "$IMAGE_TAG" \
        -f "$SCRIPT_DIR/cassandra-jolokia.Dockerfile" \
        "$SCRIPT_DIR"
else
    echo "==> Image $IMAGE_TAG already present (use --rebuild to refresh)"
fi

# Tear down the existing container.
if docker inspect "$CONTAINER_NAME" >/dev/null 2>&1; then
    echo "==> Stopping + removing existing $CONTAINER_NAME"
    docker stop "$CONTAINER_NAME" >/dev/null || true
    docker rm "$CONTAINER_NAME" >/dev/null || true
fi

# Run the new container. Anonymous volume by default; carry over
# the prior one when --keep-data was requested and a name was
# captured above.
RUN_ARGS=(
    -d
    --name "$CONTAINER_NAME"
    -p 9042:9042
    -p 8778:8778
)
if [ -n "$PRIOR_VOLUME" ]; then
    RUN_ARGS+=(-v "${PRIOR_VOLUME}:/var/lib/cassandra")
fi

echo "==> Starting $CONTAINER_NAME ($IMAGE_TAG)"
docker run "${RUN_ARGS[@]}" "$IMAGE_TAG" >/dev/null

# Wait for CQL to come up. Jolokia comes up almost immediately;
# Cassandra startup is the long pole (~20-40s on a cold node).
echo "==> Waiting for CQL on localhost:9042 ..."
for i in $(seq 1 60); do
    if (echo > /dev/tcp/127.0.0.1/9042) >/dev/null 2>&1; then
        echo "    CQL ready after ${i}s"
        break
    fi
    sleep 1
done

# Wait for Jolokia.
echo "==> Waiting for Jolokia on localhost:8778 ..."
for i in $(seq 1 60); do
    if curl -fs -m 1 http://localhost:8778/jolokia/version >/dev/null 2>&1; then
        echo "    Jolokia ready after ${i}s"
        break
    fi
    sleep 1
done

echo
echo "==> Sanity check:"
echo "  Jolokia:    $(curl -s http://localhost:8778/jolokia/version | head -c 200)"
echo
echo "Try a flush via Jolokia:"
echo "  curl -s 'http://localhost:8778/jolokia/exec/org.apache.cassandra.db:type=StorageService/forceKeyspaceFlush/baselines' | jq ."
echo
echo "Try a major compaction:"
echo "  curl -s 'http://localhost:8778/jolokia/exec/org.apache.cassandra.db:type=StorageService/forceKeyspaceCompaction/false/baselines' | jq ."
