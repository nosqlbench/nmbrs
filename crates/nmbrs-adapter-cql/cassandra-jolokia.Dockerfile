# Cassandra + Jolokia JVM agent.
#
# Adds the Jolokia JVM agent to a stock cassandra image so JMX
# operations (flush, compact, take snapshot, ...) are reachable
# over an HTTP/JSON REST surface on port 8778. Workloads can
# then trigger compaction programmatically via CQL phases that
# `INSERT INTO http(...)` (or via a side-channel script) rather
# than the JMX/RMI dance.
#
# Build:
#   docker build -t cassandra-jolokia:latest \
#     -f crates/nmbrs-adapter-cql/cassandra-jolokia.Dockerfile \
#     crates/nmbrs-adapter-cql
#
# Run (replaces the stock `cassandra-test`):
#   docker stop cassandra-test || true
#   docker rm cassandra-test || true
#   docker run -d --name cassandra-test \
#     -p 9042:9042 -p 8778:8778 \
#     cassandra-jolokia:latest
#
# Verify:
#   curl http://localhost:8778/jolokia/version
#
# Trigger a flush via Jolokia REST:
#   curl -s "http://localhost:8778/jolokia/exec/org.apache.cassandra.db:type=StorageService/forceKeyspaceFlush/<keyspace>" \
#     | jq .
#
# Trigger compaction:
#   curl -s "http://localhost:8778/jolokia/exec/org.apache.cassandra.db:type=StorageService/forceKeyspaceCompaction/false/<keyspace>" \
#     | jq .

ARG CASSANDRA_TAG=latest
FROM cassandra:${CASSANDRA_TAG}

ARG JOLOKIA_VERSION=1.7.2
# Pinned to 1.7.x because 2.x's premain blows up on hosts /
# containers without IPv6: `StaticConfiguration.initializeFromNetwork`
# unconditionally calls `getHostAddress()` on a `null` Inet6Address
# and NPEs (Jolokia issue around 2.0+). 1.7.2 is the last 1.x
# release; stable, well-tested, and exposes the JMX operations we
# need (StorageService#forceKeyspaceFlush /
# forceKeyspaceCompaction). The 1.x publish layout is the plain
# `jolokia-jvm-<ver>.jar` (which already carries the
# `Premain-Class` manifest attribute) — there's no separate
# `-agent` classifier in 1.x.
ARG JOLOKIA_URL=https://repo1.maven.org/maven2/org/jolokia/jolokia-jvm/${JOLOKIA_VERSION}/jolokia-jvm-${JOLOKIA_VERSION}.jar

USER root
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends curl ca-certificates; \
    mkdir -p /opt/jolokia; \
    curl -fL --retry 3 -o /opt/jolokia/jolokia-agent-jvm.jar "${JOLOKIA_URL}"; \
    apt-get purge -y --auto-remove curl; \
    rm -rf /var/lib/apt/lists/*

# JVM_EXTRA_OPTS is appended to the JVM startup line by
# `cassandra-env.sh`. The agent listens on all interfaces and
# binds to 8778 with no authentication — fine for a single-host
# benchmark sandbox; harden before any networked deployment.
#
# `-Djava.net.preferIPv4Stack=true` is the workaround for a
# Jolokia 2.x init bug: `StaticConfiguration.initializeFromNetwork`
# NPEs when iterating interfaces if no Inet6Address is present
# (common in containers without IPv6). Forcing the IPv4 stack
# stops the JVM from registering any Inet6Address, sidestepping
# the buggy lambda. The flag is a no-op for Jolokia 1.x (which
# doesn't have this code path).
ENV JVM_EXTRA_OPTS="-Djava.net.preferIPv4Stack=true -javaagent:/opt/jolokia/jolokia-agent-jvm.jar=port=8778,host=0.0.0.0"

EXPOSE 8778
