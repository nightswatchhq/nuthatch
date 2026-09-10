# Drop the composite check inside a to-one traversal, so a child's own relation is emitted as a scalar
# column and a stored id comes back under a field the schema declares as an object.
p = "src/graph_query.rs"; s = open(p).read()
old = """            if let Some(inner_target) = cf.ty.entity_name() {
                return Err(Unsupported::Syntax(format!(
                    "`{target}.{}` returns `{inner_target}`, which needs a selection set",
                    s.name
                )));
            }
"""
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, ""))
