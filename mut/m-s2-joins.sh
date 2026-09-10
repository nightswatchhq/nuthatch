#!/bin/zsh
# Mutations for RFC-0053 S2's relation traversal and the declaration-oriented schema parse.
#
# One Python file per mutation in `mut/s2/`, rather than a patch script quoted inside a shell string.
# Three patches in this set failed to apply on escaping alone before it was restructured this way, and
# a patch that does not apply tests unmutated code.
set -u
cd /Users/pepe/Projects/nuthatch-1265 || exit 1
# `git diff --quiet` rather than `git status --porcelain`: the apply/restore cycle touches mtimes, so
# status can report a file modified whose content matches HEAD, and the run then refuses for nothing.
git diff --quiet -- src tests || { echo "REFUSING: uncommitted work in src/tests"; exit 1; }
export PATH="$HOME/.cargo/bin:$PATH" CARGO_HOME=/Users/pepe/Projects/nuthatch-1212/cargo-home
export CARGO_TARGET_DIR=/Users/pepe/Projects/nuthatch-1265/target-1265

for m in mut/s2/*.py; do
  name=${m:t:r}
  if ! python3 "$m"; then
    echo "[$name] PATCH FAILED"; git checkout -- src; continue
  fi
  if git diff --quiet -- src; then
    echo "[$name] NOT APPLIED (no diff) - the anchor matched nothing that matters"; git checkout -- src; continue
  fi
  echo "[$name] applied: $(git diff --shortstat -- src)"
  # The whole `serve::tests::` module, not one named test: the HTTP test was split into three and a
  # filter naming only the first left two thirds of the assertions outside this gate, which showed up
  # as a GREEN mutation whose own test simply never ran.
  out=$(cargo test --offline --lib graph_ 2>&1
        cargo test --offline --lib serve::tests:: 2>&1
        cargo test --offline --test graph_schema_golden 2>&1)
  # A mutation that does not COMPILE is red too, and `error: argument never used` carries no
  # bracketed code - a `^error\[` pattern read exactly that case as GREEN.
  if echo "$out" | grep -qE "^test result: FAILED|panicked at|^error(\[|:)|^warning: unused"; then
    echo "[$name] RED: $(echo "$out" | grep -oE '(panicked at src/[^ ]+|^error: .*)' | sort -u | head -3 | tr '\n' ' ')"
  else
    echo "[$name] GREEN  <-- FINDING: no test sees this"
  fi
  git checkout -- src
done
echo "MUTATION_SET_COMPLETE"
