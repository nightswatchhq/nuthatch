p = "src/graph_query.rs"; s = open(p).read()
old = """                if let Some(inner_target) = cf.ty.entity_name() {
                    return Err(Unsupported::Syntax(format!(
                        "`{target}.{}` returns `{inner_target}`, which needs a selection set",
                        sub.name
                    )));
                }
"""
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, ""))
