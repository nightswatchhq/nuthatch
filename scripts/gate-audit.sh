#!/usr/bin/env bash
# #913: does each gate actually watch what it claims?
#
# The method is mutation, not inspection, because inspection has already failed: three of the six
# gates in #913 were READ while broken and looked fine. For each gate this drifts the artefact the
# gate exists to guard - a doc claim, a config key, a required-check name - and asserts the gate
# goes red. A gate that stays green is not a gate.
#
# Two rules learned the hard way and enforced here:
#   * mutate the GUARDED THING, never the assertion. Deleting an assertion leaves a test green by
#     construction and proves nothing.
#   * verify the patch APPLIED. A mutation that silently fails to apply tests unmutated code and
#     reports a pass.
#
# Usage: scripts/gate-audit.sh [name-filter]
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
FILTER=""
CHECK_ONLY=0
for a in "$@"; do
  case "$a" in
    --check) CHECK_ONLY=1 ;;   # verify every case still has a target; do not run cargo
    *) FILTER="$a" ;;
  esac
done
PASS=0; SURVIVED=0; SKIPPED=0
declare -a SURVIVORS

# name | test-target | file to mutate | needle | replacement
CASES=(
# Each mutation targets what the gate ACTUALLY asserts, read out of the test rather than guessed.
# The first draft of this list guessed, and produced three "survivors" that were all my own aim being
# off - a false finding is worse than none, so every case here names the assertion it provokes.

# ivm_claims asserts CLAUDE.md contains "shipped 2026-08-28" (tests/ivm_claims.rs:104).
"ivm_claims|ivm_claims|CLAUDE.md|shipped 2026-08-28|shipped 2027-01-01"
# ivm_claims also asserts CLAUDE.md says authored views are "Not incremental" (:112).
"ivm_claims_views|ivm_claims|CLAUDE.md|Not incremental|Perfectly incremental"
# rfc_index_status compares the index's STATUS COLUMN against each RFC's own status line.
"rfc_index_status|rfc_index_status|docs/rfcs/README.md|**Implemented**|**Proposed**"
# doc_command_check resolves every backticked `nuthatch ...` against clap's command tree.
"doc_command_check|doc_command_check|README.md|nuthatch dev|nuthatch frobnicate"
# required_checks reads the real .github/required-checks.txt.
"required_checks|required_checks|.github/required-checks.txt|fmt|fmtx"
# skill_refs asserts every flag named in the skill pages is a real clap flag.
# skill_refs scans AUTHORED skill pages for flags that do not exist in clap's tree, and separately
# checks the generated cli-reference.md is not stale. Two cases, because they are two mechanisms.
"skill_refs_authored|skill_refs|skills/nuthatch-builder/workflows.md|--dir|--blancmange"
"skill_refs_stale|skill_refs|skills/nuthatch-builder/cli-reference.md|--abi|--abbi"
# tape_clean guards the recorded benchmark tapes against silent edits.
# tape_clean guards exactly one thing: no recorded error in the CLEAN tape. Mutating anything else
# in that file is not a mutation of what it claims - the gate is narrow on purpose and says so.
"tape_clean|tape_clean|docs/bench/tapes/usdc-120-fixed-clean/entries.jsonl|\"outcome\":\"ok\"|\"outcome\":\"err\""
# actions_are_pinned guards two things: that no action runs by a mutable tag, and (#928) that the two
# dtolnay/rust-toolchain pins stay distinct, since that action takes its toolchain from the ref name.
"actions_pinned_tag|actions_are_pinned|.github/workflows/ci.yml|actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4|actions/checkout@v4"
"actions_pinned_toolchain|actions_are_pinned|.github/workflows/ci.yml|rust-toolchain@7c8d7d138f5c09cef361f8214cf96882cd029cdb|rust-toolchain@a5f673d0ba8626c3977bb416a1612774bc82181b"
)


for c in "${CASES[@]}"; do
  IFS='|' read -r name target file needle repl <<< "$c"
  [ -n "$FILTER" ] && [[ "$name" != *"$FILTER"* ]] && continue
  if [ ! -f "$file" ]; then
    printf "  %-28s SKIP   (%s does not exist)\n" "$name" "$file"; SKIPPED=$((SKIPPED+1)); continue
  fi
  if ! grep -qF -- "$needle" "$file"; then
    printf "  %-28s SKIP   (needle %q absent from %s - the audit case has drifted)\n" "$name" "$needle" "$file"
    SKIPPED=$((SKIPPED+1)); continue
  fi
  cp "$file" "/tmp/gate-audit.bak.$$"
  python3 - "$file" "$needle" "$repl" <<'PY'
import pathlib, sys
p = pathlib.Path(sys.argv[1]); s = p.read_text()
assert sys.argv[2] in s, "NOT APPLIED"
p.write_text(s.replace(sys.argv[2], sys.argv[3], 1))
PY
  if [ $? -ne 0 ]; then
    printf "  %-28s SKIP   (mutation did not apply)\n" "$name"; SKIPPED=$((SKIPPED+1))
    cp "/tmp/gate-audit.bak.$$" "$file"; continue
  fi
  if [ "$CHECK_ONLY" = 1 ]; then
    cp "/tmp/gate-audit.bak.$$" "$file"; rm -f "/tmp/gate-audit.bak.$$"
    printf "  %-28s target present in %s\n" "$name" "$file"; PASS=$((PASS+1)); continue
  fi
  out=$(cargo test --quiet --test "$target" 2>&1)
  rc=$?
  cp "/tmp/gate-audit.bak.$$" "$file"; rm -f "/tmp/gate-audit.bak.$$"
  if [ $rc -ne 0 ]; then
    printf "  %-28s caught (mutating %s turned it red)\n" "$name" "$file"; PASS=$((PASS+1))
  else
    printf "  %-28s SURVIVED  <-- mutating %s left it GREEN\n" "$name" "$file"
    SURVIVED=$((SURVIVED+1)); SURVIVORS+=("$name: $file :: $needle -> $repl")
  fi
done

echo
echo "caught: $PASS   survived: $SURVIVED   skipped: $SKIPPED"
if [ ${#SURVIVORS[@]} -gt 0 ]; then
  echo
  echo "SURVIVORS - these gates did not notice their own subject changing:"
  printf '  %s\n' "${SURVIVORS[@]}"
fi
# A SKIP is not benign. It means the artefact this case mutated has changed shape, so the audit has
# silently stopped covering that gate - the exact decay #913 is about, reappearing in the tool built
# to detect it. Skips fail the run.
[ "$SURVIVED" -eq 0 ] && [ "$SKIPPED" -eq 0 ]
