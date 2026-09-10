#!/bin/zsh
# Mutations for RFC-0053 S2's relation traversal and the declaration-oriented schema parse.
set -u
cd /Users/pepe/Projects/nuthatch-1265 || exit 1
[ -z "$(git status --porcelain -- src tests)" ] || { echo "REFUSING: uncommitted work in src/tests"; exit 1; }
export PATH="$HOME/.cargo/bin:$PATH" CARGO_HOME=/Users/pepe/Projects/nuthatch-1212/cargo-home
export CARGO_TARGET_DIR=/Users/pepe/Projects/nuthatch-1265/target-1265

run () {
  name="$1"; file="$2"; py="$3"
  python3 -c "$py" || { echo "[$name] PATCH FAILED"; git checkout -- "$file"; return; }
  if git diff --quiet -- "$file"; then echo "[$name] NOT APPLIED"; git checkout -- "$file"; return; fi
  echo "[$name] applied: $(git diff --shortstat -- "$file")"
  out=$(cargo test --offline --lib graph_ 2>&1; cargo test --offline --lib serve::tests::a_client_can_introspect 2>&1; cargo test --offline --test graph_schema_golden 2>&1)
  if echo "$out" | grep -qE "^test result: FAILED|panicked at|^error\["; then
    echo "[$name] RED (good): $(echo "$out" | grep -oE 'panicked at [^ ]+' | head -1)"
  else
    echo "[$name] GREEN  <-- FINDING: no test sees this"
  fi
  git checkout -- "$file"
}

run inner-join-drops-the-parent src/graph_query.rs '
p="src/graph_query.rs"; s=open(p).read()
old=" LEFT JOIN"
assert s.count(old)>=1
open(p,"w").write(s.replace(old," INNER JOIN"))'

run join-on-the-wrong-column src/graph_query.rs '
p="src/graph_query.rs"; s=open(p).read()
old="{alias}.\\"id\\" = {BASE}.\\"{}\\""
assert s.count(old)==1, s.count(old)
open(p,"w").write(s.replace(old,"{alias}.\\"id\\" = {BASE}.\\"id\\""))'

run derived-lists-join-too src/graph_query.rs '
p="src/graph_query.rs"; s=open(p).read()
old="""            (graph_schema::FieldType::Entity(t), None) => t.clone(),
            _ => return Err(Unsupported::NestedSelection(sel.name.clone())),"""
new="""            _ => match field.ty.entity_name() {
                Some(t) => t.to_string(),
                None => return Err(Unsupported::NestedSelection(sel.name.clone())),
            },"""
assert s.count(old)==1
open(p,"w").write(s.replace(old,new))'

run traversal-two-levels-deep src/graph_query.rs '
p="src/graph_query.rs"; s=open(p).read()
old="const MAX_TRAVERSAL: usize = 1;"
assert s.count(old)==1
open(p,"w").write(s.replace(old,"const MAX_TRAVERSAL: usize = 2;"))'

run order-by-unqualified src/graph_query.rs '
p="src/graph_query.rs"; s=open(p).read()
old="\" ORDER BY {BASE}.\\\\\"{order_field}\\\\\" {dir}\""
assert s.count(old)==1, s.count(old)
open(p,"w").write(s.replace(old,"\" ORDER BY \\\\\"{order_field}\\\\\" {dir}\""))'

run missing-relation-is-an-object-of-nulls src/serve.rs '
p="src/serve.rs"; s=open(p).read()
old="""                    if any {
                        serde_json::Value::Object(inner)
                    } else {
                        serde_json::Value::Null
                    },"""
assert s.count(old)==1
open(p,"w").write(s.replace(old,"                    serde_json::Value::Object(inner),"))'

run fields-are-line-oriented-again src/graph_schema.rs '
p="src/graph_schema.rs"; s=open(p).read()
old="""        if b[i].is_ascii_whitespace() || b[i] == b\x27,\x27 {"""
new="""        if b[i] == b\x27\\n\x27 {
            i += 1;
            continue;
        }
        if b[i].is_ascii_whitespace() || b[i] == b\x27,\x27 {"""
assert s.count(old)==1
# and stop after the first field on each line: skip to the newline once one is pushed
old2="""            derived_from: derived_target(tail),
        });
    }
    out
}"""
new2="""            derived_from: derived_target(tail),
        });
        while i < b.len() && b[i] != b\x27\\n\x27 {
            i += 1;
        }
    }
    out
}"""
assert s.count(old2)==1
open(p,"w").write(s.replace(old,new).replace(old2,new2))'

run enums-are-line-oriented-again src/graph_schema.rs '
p="src/graph_schema.rs"; s=open(p).read()
old="""        .flat_map(|l| strip_comment(l).split_whitespace())"""
new="""        .map(|l| strip_comment(l).trim())"""
assert s.count(old)==1
open(p,"w").write(s.replace(old,new))'

echo "MUTATION_SET_COMPLETE"
