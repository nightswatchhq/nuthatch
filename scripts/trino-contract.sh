#!/usr/bin/env bash
# RFC-0052 S3 (#1261): the Trino half of the contract test. `tests/e2e_trino_contract.rs` publishes
# a nest with a drifted table to MinIO and writes what DuckDB reads over the local segments; this
# script points a running Trino (container `trino`, Hive catalog `hive`) at the published prefix and
# fails on any difference.
#
# Usage: scripts/trino-contract.sh <fixture.json>
# Needs: jq, sha256sum, the AWS CLI (path-style, `AWS_ENDPOINT` set) and `docker`.
set -euo pipefail

fixture=${1:?usage: trino-contract.sh <fixture.json>}
container=${TRINO_CONTAINER:-trino}
endpoint=${AWS_ENDPOINT:?AWS_ENDPOINT must point at the S3-compatible store}

bucket=$(jq -er .bucket "$fixture")
prefix=$(jq -er .prefix "$fixture")
dataset=$(jq -er .dataset "$fixture")
root="s3://$bucket/$prefix/$dataset"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

trino() {
  docker exec "$container" trino --output-format=TSV "$@"
}

# §3.5: accept publish.json only when it names the catalogue actually there.
aws --endpoint-url "$endpoint" s3 cp --quiet "$root/publish.json" "$work/publish.json"
aws --endpoint-url "$endpoint" s3 cp --quiet "$root/manifest.json" "$work/manifest.json"
claimed=$(jq -er .catalogue_sha256 "$work/publish.json")
actual=$(sha256sum "$work/manifest.json" | cut -d' ' -f1)
if [ "$claimed" != "$actual" ]; then
  echo "::error::publish.json catalogue_sha256=$claimed but sha256(manifest.json)=$actual"
  exit 1
fi
echo "commit check: catalogue_sha256 $actual matches manifest.json"

for table in $(jq -r '.tables | keys[]' "$fixture"); do
  if ! jq -e --arg t "$table" '.tables | index($t)' "$work/publish.json" > /dev/null; then
    echo "::error::publish.json does not list $table"
    exit 1
  fi
done

# Column order is the latest segment's (`BTreeSet`), which is what an operator inferring a schema
# from the newest file would declare. `sender` exists only in the drifted table's second seal.
columns='"_seq" bigint, address varchar, block_hash varchar, block_number bigint,
  block_timestamp bigint, log_index bigint'
tail_columns='"table" varchar, tx_hash varchar, value varchar'
declare -A ddl=(
  [usdc__transfer]="$columns, $tail_columns"
  [usdc__drift]="$columns, sender varchar, $tail_columns"
  [usdc__wide]="$columns, $tail_columns"
)

trino --execute "CREATE SCHEMA IF NOT EXISTS hive.nuthatch"
for table in $(jq -r '.tables | keys[]' "$fixture"); do
  [ -n "${ddl[$table]:-}" ] || { echo "::error::no DDL for fixture table $table"; exit 1; }
  trino --execute "CREATE TABLE hive.nuthatch.$table (${ddl[$table]})
    WITH (external_location = '$root/$table/', format = 'PARQUET')"
done

query() {
  local table=$1 sender=0
  [[ ${ddl[$table]} == *sender* ]] && sender='count(sender)'
  shift
  trino "$@" --execute "SELECT count(*), CAST(sum(try_cast(value AS decimal(38,0))) AS varchar),
    $sender FROM hive.nuthatch.$table"
}

failed=0
for table in $(jq -r '.tables | keys[]' "$fixture"); do
  want=$(jq -er --arg t "$table" '.tables[$t] | "\(.count)\t\(.sum)\t\(.count_sender)"' "$fixture")
  got=$(query "$table")
  printf '%s\n  duckdb (local): %s\n  trino (prefix): %s\n' "$table" "$want" "$got"
  if [ "$got" != "$want" ]; then
    echo "::error::$table: Trino returned [$got], local DuckDB [$want] (count, sum, count(sender))"
    failed=1
  fi
done

# RFC-0055 S3 (#1359): each view `emit dune` translated, pointed at this catalogue, must return the
# rows the nest's own DuckDB view returned. A fixture that carries no views fails rather than passes.
[ "$(jq '.views // {} | length' "$fixture")" -gt 0 ] || { echo "::error::the fixture carries no translated views"; exit 1; }
for view in $(jq -r '.views | keys[]' "$fixture"); do
  want=$(jq -r --arg v "$view" '.views[$v].lines[]' "$fixture")
  if ! got=$(trino --execute "$(jq -er --arg v "$view" '.views[$v].trino_sql' "$fixture")" | LC_ALL=C sort); then
    echo "::error::view $view: Trino refused the translated query"
    failed=1
    continue
  fi
  printf '%s\n  duckdb (nest view):\n%s\n  trino (translated):\n%s\n' "$view" "$want" "$got"
  if [ "$got" != "$want" ]; then
    echo "::error::view $view: Trino and the nest's own DuckDB view returned different rows"
    failed=1
  fi
done
[ "$failed" -eq 0 ] || exit 1

# The drifted table has to be able to fail this test. Read by index, it must come out wrong.
want=$(jq -er '.tables.usdc__drift | "\(.count)\t\(.sum)\t\(.count_sender)"' "$fixture")
by_index=$(query usdc__drift --session hive.parquet_use_column_names=false)
printf 'usdc__drift by index (parquet_use_column_names=false): %s\n' "$by_index"
if [ "$by_index" = "$want" ]; then
  echo "::error::usdc__drift reads correctly by column index, so it no longer tests use-column-names"
  exit 1
fi
