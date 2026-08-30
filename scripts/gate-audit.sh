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
#   * (#974) a RED RESULT IS NOT A CAUGHT MUTATION. Until 2026-08-30 this script read any nonzero
#     `cargo test` exit as "caught", so a missing test target, a compile error or an already-red
#     baseline all reported success - the audit's own failure mode, in the tool built to find it.
#     Now every target is run UNMUTATED first and must be green, must compile under mutation, and
#     must fail as an assertion rather than as a build error.
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
declare -a SURVIVORS=()

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
"actions_pinned_tag|actions_are_pinned|.github/workflows/ci.yml|uses: actions/checkout@|uses: actions/checkout@v4 # "
"actions_pinned_toolchain|actions_are_pinned|.github/workflows/ci.yml|rust-toolchain@7c8d7d138f5c09cef361f8214cf96882cd029cdb|rust-toolchain@a5f673d0ba8626c3977bb416a1612774bc82181b"
)


# --- #974: tell an assertion failure apart from a setup failure ---------------------------------
#
# `cargo test` exits nonzero for a compile error, a missing target and a failing assertion alike.
# Only the last is a caught mutation. Compilation is checked separately, and the run must announce
# a test result rather than merely failing.
BASELINE_OK=""     # targets proven green unmutated, space-delimited
BASELINE_BAD=""

target_exists() {  # target_exists <name>
  [ -f "tests/$1.rs" ]
}

# Prints one of: pass | assert-fail | build-fail | no-tests
#
# ORDER MATTERS, and the first draft of this function got it wrong in a way worth recording. It
# tested for `^error:` first, but a perfectly ordinary assertion failure ends with
# `error: test failed, to rerun pass --test <name>` - so every genuine catch was classified
# `build-fail` and skipped, which would have made the whole audit unreportable while looking
# deliberate. The harness must be able to tell "the test ran and failed" from "the test never ran",
# and the only reliable evidence that it RAN is a `test result:` line.
classify_run() {   # classify_run <cargo test output> <exit code>
  local out="$1" rc="$2"
  if printf '%s' "$out" | grep -qE '^test result:'; then
    if [ "$rc" -eq 0 ]; then printf 'pass'; else printf 'assert-fail'; fi
    return
  fi
  # It did not run. Distinguish "would not build" from "built, but has no tests".
  if printf '%s' "$out" | grep -qE '^error\[E[0-9]+\]:|could not compile|^error: expected|^error: unexpected'; then
    printf 'build-fail'; return
  fi
  printf 'no-tests'
}

# Every distinct target, run once, unmutated. A case whose target is already red cannot report
# anything: its mutation would be "caught" by a failure that was there before the mutation.
if [ "$CHECK_ONLY" = 0 ]; then
  seen=""
  for c in "${CASES[@]}"; do
    IFS='|' read -r _n t _rest <<< "$c"
    case " $seen " in *" $t "*) continue ;; esac
    seen="$seen $t"
    if ! target_exists "$t"; then
      echo "  BASELINE  $t: no tests/$t.rs - the case names a target that does not exist"
      BASELINE_BAD="$BASELINE_BAD $t"; continue
    fi
    bout=$(cargo test --quiet --test "$t" 2>&1); brc=$?
    bkind=$(classify_run "$bout" "$brc")
    if [ "$bkind" = pass ]; then
      BASELINE_OK="$BASELINE_OK $t"
    else
      echo "  BASELINE  $t: $bkind (unmutated). Every case on this target is unreportable until it is green."
      BASELINE_BAD="$BASELINE_BAD $t"
    fi
  done
  echo
fi

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
  # #974: replace EVERY occurrence, not the first.
  #
  # The guarded property is "this file still asserts X". Replacing one of several instances leaves
  # the property intact, the gate correctly green, and the audit reporting SURVIVED - a false
  # finding against a gate that is working, which this script's own header calls worse than none.
  # It happened: `shipped 2026-08-28` occurs twice in CLAUDE.md, and `ivm_claims` was reported as a
  # survivor on 2026-08-30 for exactly that reason. The count is printed so a needle matching far
  # more than intended is visible rather than silent.
  hits=$(python3 - "$file" "$needle" "$repl" <<'PYMUT'
import pathlib, sys
p = pathlib.Path(sys.argv[1]); s = p.read_text()
n = s.count(sys.argv[2])
assert n > 0, "NOT APPLIED"
p.write_text(s.replace(sys.argv[2], sys.argv[3]))
print(n)
PYMUT
  )
  if [ $? -ne 0 ]; then
    printf "  %-28s SKIP   (mutation did not apply)\n" "$name"; SKIPPED=$((SKIPPED+1))
    cp "/tmp/gate-audit.bak.$$" "$file"; continue
  fi
  if [ "${hits:-0}" -gt 1 ]; then
    printf "  %-28s note   (needle occurs %s times; all replaced)\n" "$name" "$hits"
  fi
  if [ "$CHECK_ONLY" = 1 ]; then
    cp "/tmp/gate-audit.bak.$$" "$file"; rm -f "/tmp/gate-audit.bak.$$"
    printf "  %-28s target present in %s\n" "$name" "$file"; PASS=$((PASS+1)); continue
  fi
  out=$(cargo test --quiet --test "$target" 2>&1)
  rc=$?
  cp "/tmp/gate-audit.bak.$$" "$file"; rm -f "/tmp/gate-audit.bak.$$"
  kind=$(classify_run "$out" "$rc")
  case " $BASELINE_OK " in
    *" $target "*) ;;
    *) printf "  %-28s SKIP   (%s was not green unmutated - see BASELINE above)\n" "$name" "$target"
       SKIPPED=$((SKIPPED+1)); continue ;;
  esac
  if [ "$kind" = build-fail ] || [ "$kind" = no-tests ]; then
    # #974: this used to read as "caught". It is the audit failing, not the gate working.
    printf "  %-28s SKIP   (mutation produced %s, not an assertion failure)\n" "$name" "$kind"
    SKIPPED=$((SKIPPED+1)); continue
  fi
  if [ "$kind" = assert-fail ]; then
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
# A bad baseline is a failure of the audit, not of a gate, and must not be reported as a clean run.
if [ -n "$BASELINE_BAD" ]; then
  echo
  echo "UNREPORTABLE - these targets were not green unmutated, so nothing measured against them counts:"
  printf '  %s\n' $BASELINE_BAD
fi
[ "$SURVIVED" -eq 0 ] && [ "$SKIPPED" -eq 0 ] && [ -z "$BASELINE_BAD" ]
