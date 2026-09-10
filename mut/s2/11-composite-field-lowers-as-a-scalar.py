# Drop the refusal, so `{ pools { token0 } }` returns the relation id under a field the schema
# declares as an object.
p = "src/graph_query.rs"; s = open(p).read()
old = """            if let Some(target) = field.ty.entity_name() {
                return Err(Unsupported::Syntax(format!(
                    "`{}.{}` returns `{target}`, which needs a selection set",
                    entity, sel.name
                )));
            }
"""
assert s.count(old) == 1
open(p, "w").write(s.replace(old, ""))
