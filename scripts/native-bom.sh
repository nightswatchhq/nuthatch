#!/usr/bin/env bash
# RFC-0042 slice 0: the native bill of materials.
#
# Per release target: which crates compile C, C++ or assembly INTO the binary; why each is in the
# graph; what it costs in clean-build time and output size.
#
# Every figure is produced from the build, never from a crate name. `duckdb` sounding like C++ is not
# evidence; object files under its build directory are.
#
# **Fails loudly on a bad precondition.** The first version of this script `cd`'d to the wrong
# directory, ran there anyway, and produced a clean-looking report full of blanks - the same shape as
# the build script that silently compiled a stale tree the day before.
set -euo pipefail

# --- #975: portable file size -------------------------------------------------------------------
#
# GNU `stat -c%s` and BSD/macOS `stat -f%z` are not interchangeable, and the failure is silent
# rather than loud: the wrong flag prints an error to stderr and an empty string to stdout, so a
# size lands in the report as blank or zero. Detected once, here, rather than assumed from the
# platform name.
if stat -c%s . >/dev/null 2>&1; then
  file_size() { stat -c%s "$1"; }        # GNU coreutils
elif stat -f%z . >/dev/null 2>&1; then
  file_size() { stat -f%z "$1"; }        # BSD / macOS
else
  echo "cannot determine a working 'stat' size flag on this system" >&2; exit 1
fi


REPO=${REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
[ -d "$REPO/.git" ] || { echo "REPO=$REPO is not a git checkout" >&2; exit 1; }
[ -f "$REPO/Cargo.toml" ] || { echo "REPO=$REPO has no Cargo.toml" >&2; exit 1; }
cd "$REPO"

OUT=${OUT:-/tmp/native-bom}
mkdir -p "$OUT"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$OUT/target}

echo "=== $(date -u +%FT%TZ) ==="
echo "repo:   $REPO"
echo "commit: $(git log --oneline -1)"
echo "rustc:  $(rustc --version)"
echo "host:   $(uname -srm)"
# The pin is load-bearing: dbsp ICEs on 1.97, and a BOM built on a different compiler is a BOM of a
# different binary.
if [ -f rust-toolchain.toml ]; then echo "pinned: $(grep -m1 channel rust-toolchain.toml)"; fi
echo

echo "=== clean release build ==="
rm -rf "$CARGO_TARGET_DIR"
start=$SECONDS
cargo build --release --locked --timings > "$OUT/build.out" 2> "$OUT/build.err" || {
  echo "BUILD FAILED - last 15 lines:"; tail -15 "$OUT/build.err"; exit 1; }
echo "clean release build: $((SECONDS - start))s wall"

bin="$CARGO_TARGET_DIR/release/nuthatch"
[ -x "$bin" ] || { echo "no binary at $bin"; exit 1; }

echo
echo "=== crates that invoked a C/C++ compiler or assembler ==="
echo "(counted by object files produced under each crate's build dir - the ground truth)"
find "$CARGO_TARGET_DIR/release/build" \( -name '*.o' -o -name '*.obj' \) 2>/dev/null \
  | sed -E 's#.*/build/([^/]+)/.*#\1#' | sed -E 's/-[0-9a-f]{16}$//' \
  | sort | uniq -c | sort -rn

echo
echo "=== native artefact bytes per crate ==="
for d in "$CARGO_TARGET_DIR"/release/build/*/out; do
  [ -d "$d" ] || continue
  # #975: `find -printf` is GNU-only and silently prints nothing on BSD/macOS find, which would
  # report every crate as 0 bytes rather than failing. `-exec stat` with a portable size flag works
  # on both; `bom-mac.sh` is the macOS entry point but this must not lie if it is run there.
  # #975: `find -printf '%s'` is GNU-only, and on BSD/macOS find it fails to stdout-nothing rather
  # than erroring - so every crate would report 0 native bytes and the BOM would publish that as a
  # finding. Concatenating and counting needs no `stat` flag and behaves identically on both.
  total=$(find "$d" \( -name '*.o' -o -name '*.a' \) -exec cat {} + 2>/dev/null | wc -c | tr -d ' ')
  if [ "${total:-0}" -gt 0 ]; then
    printf "%14s  %s\n" "$total" "$(basename "$(dirname "$d")" | sed -E 's/-[0-9a-f]{16}$//')"
  fi
done | sort -rn | head -20

echo
echo "=== final binary ==="
printf "%14s  bytes (stripped: %s)\n" "$(file_size "$bin")" "$(file "$bin" | grep -o 'not stripped\|stripped' | head -1)"
echo
echo "timings: $CARGO_TARGET_DIR/cargo-timings/cargo-timing.html"
echo "=== BOM OK ==="
