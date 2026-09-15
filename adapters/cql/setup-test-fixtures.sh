#!/bin/bash
# Generate a tiny synthetic CQL test-dataset fixture and register it
# in vectordata's catalog so `nmbrs run workload=full_cql_vector.yaml`
# (and other workloads that default to `dataset=example`) can resolve
# the dataset name without needing a network catalog.
#
# Output lives under target/test-fixtures/ — gitignored, regenerated
# on every run, never committed.
#
# Requirements:
#   - veks installed (`cargo install veks`)
#   - workspace built at least once (so target/ exists)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
FIXTURES="$PROJECT_ROOT/target/test-fixtures"

if ! command -v veks >/dev/null 2>&1; then
    echo "veks not found. Install with:"
    echo "  cargo install veks"
    exit 1
fi

mkdir -p "$FIXTURES"

# ─── Generate a minimal `example` dataset ───
# 64 base vectors × dim=8, 16 queries, 4-NN ground truth, COSINE.
# Adequate for the ANN phases of full_cql_vector.yaml; insufficient
# for fknn / pvs phases (those need metadata_content / predicate /
# filtered-neighbor facets that the basic generator doesn't emit).

echo "==> Generating synthetic dataset → $FIXTURES/example/"
rm -rf "$FIXTURES/example"
veks pipeline generate dataset \
    --output-dir "$FIXTURES/example" \
    --dimension 8 \
    --base-count 64 \
    --query-count 16 \
    --neighbors 4 \
    --distance COSINE \
    --seed 42 \
    --force true \
    >/dev/null

# Default name is "generated-dataset"; the workloads here default to
# `dataset=example`. Rename only the first line, preserving the rest.
{ echo "name: example"; tail -n +2 "$FIXTURES/example/dataset.yaml"; } \
    > "$FIXTURES/example/dataset.yaml.new"
mv "$FIXTURES/example/dataset.yaml.new" "$FIXTURES/example/dataset.yaml"

# ─── Build a catalog at the publish root ───
# `veks prepare catalog generate` requires a `.publish_url` sentinel
# at the catalog root and a `.publish` marker per dataset directory.
# These are publishing-pipeline artifacts; for local-only fixtures
# the values are nominal.

echo 's3://nmbrs-local/test-data/' > "$FIXTURES/.publish_url"
touch "$FIXTURES/example/.publish"

echo "==> Generating catalog files"
veks prepare catalog generate "$FIXTURES" >/dev/null

# ─── Register with vectordata ───
# Idempotent: `add-catalog` skips if the entry is already present.

echo "==> Registering with vectordata catalog"
veks datasets config add-catalog "$FIXTURES" 2>&1 | grep -v "already registered" || true

echo
echo "==> Verifying registration"
veks datasets list --dataset example 2>&1 \
    | grep -v "^ERROR: failed to load required catalog" \
    | tail -5

cat <<EOF

Setup complete. The following phases of
nmbrs/workloads/cql/full_cql_vector.yaml run cleanly under
\`dryrun=phase\` against this fixture:

  discover, schema, teardown, drop_indexes, create_index,
  build_index, rampup, ann_query

The fknn_rampup_data and pvs_query phases need
metadata_content / filtered_neighbor_indices facets that this
minimal generator does not produce.

Smoke test:

  cargo run -p nmbrs -- run \\
    adapter=cql cqldriver=scylla \\
    workload=nmbrs/workloads/cql/full_cql_vector.yaml \\
    profile=default table=vec_default optimize_for=RECALL \\
    dryrun=phase
EOF
