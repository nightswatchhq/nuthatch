#!/bin/bash
# RFC-0060 §5.6 deletion test: a default build's CLI help and `init` output must equal main's.
# Usage: rfc-0060-deletion-test.sh <main-binary> <default-build-binary> <out-dir> <main src/cli.rs>
# Hidden commands come from cli.rs, because `--help` never lists them.
set -u
REF=$1; GATED=$2; OUT=$3
mkdir -p "$OUT"; rm -rf "$OUT"/*
subs() { "$1" $2 --help 2>/dev/null | grep -E '^  [a-z][a-z0-9-]* ' | awk '{print $1}' | grep -v '^help$'; }
top="$(subs "$REF" "") $(awk '/#\[command\(hide = true\)\]/{getline; print}' "$4" | sed -E 's/^ *([A-Za-z]+).*/\1/; s/([a-z])([A-Z])/\1-\2/g' | tr A-Z a-z)"
help() {
  local bin=$1
  "$bin" --help 2>&1
  for c in $top; do
    echo "=== $c"; "$bin" $c --help 2>&1
    for s in $(subs "$bin" "$c"); do echo "=== $c $s"; "$bin" $c $s --help 2>&1; done
  done
}
help "$REF" > "$OUT/help-ref.txt"; help "$GATED" > "$OUT/help-gated.txt"
echo "top-level commands: $(echo $top | wc -w)"
echo "help pages compared: $(grep -c '^=== ' "$OUT/help-ref.txt")"
diff "$OUT/help-ref.txt" "$OUT/help-gated.txt" > "$OUT/help.diff"; echo "help-diff-lines=$(wc -l < "$OUT/help.diff" | tr -d ' ')"
ABI='[{"type":"event","name":"Transfer","anonymous":false,"inputs":[{"name":"from","type":"address","indexed":true},{"name":"to","type":"address","indexed":true},{"name":"value","type":"uint256","indexed":false}]}]'
for side in ref gated; do
  bin=$REF; [ $side = gated ] && bin=$GATED
  mkdir -p "$OUT/$side"; echo "$ABI" > "$OUT/$side/erc20.json"
  ( cd "$OUT/$side" && "$bin" init 0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48 --abi erc20.json --chain mainnet --rpc http://127.0.0.1:9 --dir nest > init.log 2>&1; echo "init-$side-exit=$?" )
done
diff -r "$OUT/ref/nest" "$OUT/gated/nest" > "$OUT/init.diff"; echo "init-diff-lines=$(wc -l < "$OUT/init.diff" | tr -d ' ')"
diff "$OUT/ref/init.log" "$OUT/gated/init.log" > "$OUT/init-log.diff"; echo "init-output-diff-lines=$(wc -l < "$OUT/init-log.diff" | tr -d ' ')"
echo "init files: $(find "$OUT/ref/nest" -type f | wc -l | tr -d ' ')"
