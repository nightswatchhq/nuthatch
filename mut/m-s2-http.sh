#!/bin/zsh
# Mutation set for RFC-0053 S2's HTTP surface and the parse/compile nesting split.
set -u
cd /Users/pepe/Projects/nuthatch-1265 || exit 1
[ -z "$(git status --porcelain -- src tests)" ] || { echo "REFUSING: uncommitted work in src/tests"; exit 1; }
export PATH="$HOME/.cargo/bin:$PATH" CARGO_HOME=/Users/pepe/Projects/nuthatch-1212/cargo-home
export CARGO_TARGET_DIR=/Users/pepe/Projects/nuthatch-1265/target-1265

run () {
  name="$1"; file="$2"; py="$3"
  python3 -c "$py" || { echo "[$name] PATCH FAILED"; git checkout -- "$file"; return; }
  if git diff --quiet -- "$file"; then echo "[$name] NOT APPLIED (no diff)"; git checkout -- "$file"; return; fi
  echo "[$name] applied: $(git diff --shortstat -- "$file")"
  out=$(cargo test --offline --lib graph_query 2>&1; cargo test --offline --lib serve::tests::a_client_can_introspect 2>&1)
  if echo "$out" | grep -qE "^test result: FAILED|panicked at|^error(\[|:)|^warning: unused"; then
    echo "[$name] RED (good)"
    echo "$out" | grep -E "panicked at|^---- |assertion" | head -3
  else
    echo "[$name] GREEN  <-- FINDING: no test sees this"
  fi
  git checkout -- "$file"
}

run drop-compile-nesting-refusal src/graph_query.rs '
p="src/graph_query.rs"; s=open(p).read()
old="""    if let Some(n) = root.nested.first() {
        return Err(Unsupported::NestedSelection(n.clone()));
    }
"""
assert s.count(old)==1
open(p,"w").write(s.replace(old,""))'

run brace-skip-no-depth src/graph_query.rs '
p="src/graph_query.rs"; s=open(p).read()
old="""                            b\x27}\x27 => {
                                depth -= 1;
                                if depth == 0 {
                                    c.i += 1;
                                    break;
                                }
                            }"""
new="""                            b\x27}\x27 => {
                                c.i += 1;
                                break;
                            }"""
assert s.count(old)==1, "depth arm"
open(p,"w").write(s.replace(old,new))'

run nested-also-counts-as-a-leaf src/graph_query.rs '
p="src/graph_query.rs"; s=open(p).read()
old="""                    nested.push(f);
                    continue;"""
new="""                    nested.push(f.clone());
                    fields.push(f);
                    continue;"""
assert s.count(old)==1
open(p,"w").write(s.replace(old,new))'

run meta-claims-indexing-errors src/serve.rs '
p="src/serve.rs"; s=open(p).read()
old="\"hasIndexingErrors\": false"
assert s.count(old)==1, s.count(old)
open(p,"w").write(s.replace(old,"\"hasIndexingErrors\": true"))'

run first-default-is-not-100 src/graph_query.rs '
p="src/graph_query.rs"; s=open(p).read()
import re
m=re.search(r"(let mut first[^=]*= )100", s)
assert m, "first default"
open(p,"w").write(s[:m.start(1)]+m.group(1)+"1000"+s[m.end():])'

echo "MUTATION_SET_COMPLETE"
