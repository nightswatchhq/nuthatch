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

# #975: this script's whole claim is "the aarch64-apple-darwin half", and it neither required nor
# selected that target. Under Rosetta, or on an Intel Mac, it would build x86_64 and publish the
# result as Apple Silicon evidence - a wrong number that looks exactly like a right one. The target
# is now named explicitly and the architecture is checked rather than assumed.
TARGET=${TARGET:-aarch64-apple-darwin}
arch_now=$(uname -m)
if [ "$TARGET" = "aarch64-apple-darwin" ] && [ "$arch_now" != "arm64" ]; then
  echo "refusing to publish aarch64-apple-darwin evidence from a $arch_now shell." >&2
  echo "  uname -m says '$arch_now'; on Apple Silicon it must say 'arm64'." >&2
  echo "  If you are inside Rosetta, exit it (\`arch -arm64 \$SHELL\`) and re-run." >&2
  echo "  To measure a different target deliberately, set TARGET=<triple>." >&2
  exit 1
fi
if ! rustc --print target-list | grep -qx "$TARGET"; then
  echo "TARGET=$TARGET is not a target this rustc knows" >&2; exit 1
fi

echo "=== $(date -u +%FT%TZ) ==="
echo "commit: $(git log --oneline -1)"
echo "rustc:  $(rustc --version)"
echo "host:   $(uname -srm)"
echo "target: $TARGET"
echo "pinned: $(grep -m1 channel rust-toolchain.toml 2>/dev/null)"
echo
echo "=== clean release build ==="
rm -rf "$CARGO_TARGET_DIR"
start=$SECONDS
cargo build --release --locked --timings --target "$TARGET" > "$OUT/build.out" 2> "$OUT/build.err" || {
  echo "BUILD FAILED:"; tail -15 "$OUT/build.err"; exit 1; }
echo "clean release build: $((SECONDS - start))s wall"
# `--target` puts artefacts under a triple subdirectory; without this the paths below would miss.
bin="$CARGO_TARGET_DIR/$TARGET/release/nuthatch"
[ -x "$bin" ] || { echo "no binary"; exit 1; }

echo
echo "=== crates that invoked a C/C++ compiler ==="
find "$CARGO_TARGET_DIR/$TARGET/release/build" -name '*.o' 2>/dev/null \
  | sed -E 's#.*/build/([^/]+)/.*#\1#' | sed -E 's/-[0-9a-f]{16}$//' | sort | uniq -c | sort -rn

echo
echo "=== native artefact bytes per crate ==="
for d in "$CARGO_TARGET_DIR/$TARGET"/release/build/*/out; do
  [ -d "$d" ] || continue
  total=$(find "$d" \( -name '*.o' -o -name '*.a' \) -exec stat -f%z {} \; 2>/dev/null | awk '{s+=$1} END{print s+0}')
  [ "${total:-0}" -gt 0 ] && printf "%14s  %s\n" "$total" "$(basename "$(dirname "$d")" | sed -E 's/-[0-9a-f]{16}$//')"
done | sort -rn | head -12

echo
echo "=== the C++ evidence, macOS form ==="
otool -L "$bin" | tail -n +2 | awk '{print "  "$1}'
echo
# The binary itself is the last word on what was built: a `file`/`lipo` check catches a build that
# succeeded for the wrong architecture, which the shell-level guard above cannot see if cargo was
# configured elsewhere (`.cargo/config.toml`, CARGO_BUILD_TARGET).
built_arch=$(lipo -archs "$bin" 2>/dev/null || file -b "$bin")
echo "built arch:   $built_arch"
case "$TARGET:$built_arch" in
  aarch64-apple-darwin:*arm64*) ;;
  x86_64-apple-darwin:*x86_64*) ;;
  *) echo "the binary is '$built_arch' but TARGET=$TARGET - this evidence is mislabelled" >&2; exit 1 ;;
esac
printf "final binary: %s bytes\n" "$(stat -f%z "$bin")"
echo "=== BOM OK ==="
