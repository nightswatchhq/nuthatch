#!/usr/bin/env bash
# Checks the stopgap orphan list's inputs against the Graph Network subgraph at one block (#1942):
# for every deployment over 1 GRT of signal, the graph-allocations-nest's net signal and active
# SubgraphService allocations, read from its published segments, must equal the subgraph's to the wei
# and to the allocation. Then counts the bsc and matic deployments with none, by the subgraph's
# manifest network. Exits 1 on any disagreement.
#
#   scripts/stopgap-orphans-verify.sh [block]
#
# The block defaults to the highest one all five tables are published through. Needs curl, jq,
# duckdb and shasum. SUBGRAPH may name another endpoint serving the network subgraph.
set -euo pipefail

mirror=${MIRROR:-https://pub-bc282d5016f242a783a6c28cbcd4401a.r2.dev}
dataset=${DATASET:-d6fd43c87dd9593872b5c0f414b7291b230d3da7b2fcdf3ba2ad49e5ec496e8b}
subgraph=${SUBGRAPH:-https://network.thenightswatch.dev/graphql}
tables='curation__signalled curation__burned curation__collected subgraph_service__allocation_created subgraph_service__allocation_closed'
work=${WORK:-$(mktemp -d)}
mkdir -p "$work/seg"
cd "$work"

curl -fsS -m 120 "$mirror/$dataset/manifest.json" -o manifest.json
# A table's tail past its last segment is still in the hot store, so only that far is published.
complete=$(jq -r --arg t "$tables" '[.tables | to_entries[] | select(.key as $k | $t | split(" ") | index($k)) | .value | map(.to_block) | max] | min' manifest.json)
B=${1:-$complete}
[ "$B" -le "$complete" ] || { echo "block $B is past $complete, the last block every table is published through" >&2; exit 1; }
echo "PIN block=$B dataset=$dataset"

for t in $tables; do
  jq -r --arg t "$t" --argjson b "$B" '.tables[$t][] | select(.from_block <= $b) | .hash' manifest.json \
    | while read -r h; do echo "url = \"$mirror/$dataset/$t/$h.parquet\""; echo "output = \"seg/$t/$h.parquet\""; done
done > curl.cfg
curl -fsS --parallel --parallel-max 8 --create-dirs --retry 3 -K curl.cfg
find seg -name '*.parquet' | while read -r f; do
  [ "$(shasum -a 256 "$f" | cut -d' ' -f1)" = "$(basename "$f" .parquet)" ] || { echo "$f does not hash to its name" >&2; exit 1; }
done

cat > nest.sql <<EOF
COPY (
WITH signal AS (
  SELECT dep, SUM(tok) AS s FROM (
    SELECT LOWER("subgraphDeploymentID") dep, CAST(tokens AS HUGEINT) - CAST("curationTax" AS HUGEINT) tok, block_number FROM 'seg/curation__signalled/*.parquet'
    UNION ALL SELECT LOWER("subgraphDeploymentID"), -CAST(tokens AS HUGEINT), block_number FROM 'seg/curation__burned/*.parquet'
    UNION ALL SELECT LOWER("subgraphDeploymentID"), CAST(tokens AS HUGEINT), block_number FROM 'seg/curation__collected/*.parquet'
  ) WHERE block_number <= $B GROUP BY 1
),
active AS (
  SELECT LOWER(c."subgraphDeploymentId") dep, COUNT(*) n
  FROM 'seg/subgraph_service__allocation_created/*.parquet' c
  WHERE c.block_number <= $B
    AND NOT EXISTS (SELECT 1 FROM 'seg/subgraph_service__allocation_closed/*.parquet' x
                    WHERE x."allocationId" = c."allocationId" AND x.block_number <= $B)
  GROUP BY 1
)
SELECT s.dep, CAST(s.s AS VARCHAR) AS signal, COALESCE(a.n, 0) AS active
FROM signal s LEFT JOIN active a USING (dep) WHERE s.s > 1000000000000000000 ORDER BY 1
) TO 'nest.csv' (HEADER false);
EOF
duckdb -f nest.sql

: > sg.csv
last=0x
while :; do
  q="{ subgraphDeployments(first: 500, orderBy: id, block: {number: $B},
       where: {id_gt: \"$last\", signalledTokens_gt: \"1000000000000000000\"}) {
       id signalledTokens manifest { network } indexerAllocations(first: 1000, where: {status: Active}) { id } } }"
  curl -fsS -m 120 -A stopgap-orphans-verify -H 'content-type: application/json' "$subgraph" \
    -d "$(jq -nc --arg q "$q" '{query: $q}')" -o page.json
  [ "$(jq '.errors | length' page.json)" = 0 ] || { jq -c .errors page.json >&2; exit 1; }
  [ "$(jq '.data.subgraphDeployments | length' page.json)" -gt 0 ] || break
  jq -r '.data.subgraphDeployments[] | [.id, (.manifest.network // ""), .signalledTokens, (.indexerAllocations | length)] | @csv' page.json >> sg.csv
  last=$(jq -r '.data.subgraphDeployments[-1].id' page.json)
done

cat > compare.sql <<'EOF'
.mode list
.headers off
.separator " "
CREATE TEMP TABLE n AS SELECT * FROM read_csv('nest.csv', header=false, columns={'dep':'VARCHAR','signal':'VARCHAR','active':'BIGINT'});
CREATE TEMP TABLE g AS SELECT * FROM read_csv('sg.csv', header=false, columns={'dep':'VARCHAR','network':'VARCHAR','signal':'VARCHAR','active':'BIGINT'});
SELECT 'deployments', (SELECT COUNT(*) FROM n), (SELECT COUNT(*) FROM g);
SELECT 'active_allocations', (SELECT SUM(active) FROM n), (SELECT SUM(active) FROM g);
SELECT 'DIFF only_nest', COUNT(*) FROM n ANTI JOIN g USING (dep) HAVING COUNT(*) > 0;
SELECT 'DIFF only_subgraph', COUNT(*) FROM g ANTI JOIN n USING (dep) HAVING COUNT(*) > 0;
SELECT 'DIFF signal', COUNT(*) FROM n JOIN g USING (dep) WHERE n.signal <> g.signal HAVING COUNT(*) > 0;
SELECT 'DIFF active', COUNT(*) FROM n JOIN g USING (dep) WHERE n.active <> g.active HAVING COUNT(*) > 0;
SELECT 'orphans', g.network, COUNT(*) FILTER (WHERE n.active = 0), COUNT(*) FILTER (WHERE g.active = 0)
FROM n JOIN g USING (dep) WHERE g.network IN ('bsc', 'matic') GROUP BY 2 ORDER BY 2;
SELECT 'no_manifest_network', COUNT(*) FILTER (WHERE n.active = 0) FROM n JOIN g USING (dep) WHERE g.network = '';
EOF
duckdb -f compare.sql | tee result.txt
if grep -q '^DIFF' result.txt; then echo "FAIL: the nest and the subgraph disagree at block $B ($work)"; exit 1; fi
echo "OK: agree at block $B ($work)"
