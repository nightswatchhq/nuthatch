#!/usr/bin/env bash
# #948: the aarch64-apple-darwin half of RFC-0042 slice 0's BOM.
#
# macOS is not a translation of the Linux run. The C++ evidence there is `libstdc++.so.6` in `ldd`;
# here it is `libc++` in `otool -L`, and the tool names differ throughout (`otool` not `objdump`,
# `stat -f%z` not `stat -c%s`). Re-established rather than assumed.
set -euo pipefail
REPO=${REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
[ -d "$REPO/.git" ] || { echo "REPO=$REPO is not a checkout" >&2; exit 1; }
cd "$REPO"
OUT=${OUT:-/tmp/bom-mac}; mkdir -p "$OUT"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$OUT/target}

echo "=== $(date -u +%FT%TZ) ==="
echo "commit: $(git log --oneline -1)"
echo "rustc:  $(rustc --version)"
echo "host:   $(uname -srm)"
echo "pinned: $(grep -m1 channel rust-toolchain.toml 2>/dev/null)"
echo
echo "=== clean release build ==="
rm -rf "$CARGO_TARGET_DIR"
start=$SECONDS
cargo build --release --locked --timings > "$OUT/build.out" 2> "$OUT/build.err" || {
  echo "BUILD FAILED:"; tail -15 "$OUT/build.err"; exit 1; }
echo "clean release build: $((SECONDS - start))s wall"
bin="$CARGO_TARGET_DIR/release/nuthatch"
[ -x "$bin" ] || { echo "no binary"; exit 1; }

echo
echo "=== crates that invoked a C/C++ compiler ==="
find "$CARGO_TARGET_DIR/release/build" -name '*.o' 2>/dev/null \
  | sed -E 's#.*/build/([^/]+)/.*#\1#' | sed -E 's/-[0-9a-f]{16}$//' | sort | uniq -c | sort -rn

echo
echo "=== native artefact bytes per crate ==="
for d in "$CARGO_TARGET_DIR"/release/build/*/out; do
  [ -d "$d" ] || continue
  total=$(find "$d" \( -name '*.o' -o -name '*.a' \) -exec stat -f%z {} \; 2>/dev/null | awk '{s+=$1} END{print s+0}')
  [ "${total:-0}" -gt 0 ] && printf "%14s  %s\n" "$total" "$(basename "$(dirname "$d")" | sed -E 's/-[0-9a-f]{16}$//')"
done | sort -rn | head -12

echo
echo "=== the C++ evidence, macOS form ==="
otool -L "$bin" | tail -n +2 | awk '{print "  "$1}'
echo
printf "final binary: %s bytes\n" "$(stat -f%z "$bin")"
echo "=== BOM OK ==="
