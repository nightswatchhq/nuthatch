# Treat an unresolved spread as nothing, silently dropping every field it carried.
p = "src/graph_query.rs"; s = open(p).read()
old = """                let Some(body) = fragments.get(name) else {
                    return Err(Unsupported::Syntax(format!(
                        "fragment `{name}` is spread but never defined"
                    )));
                };"""
new = """                let Some(body) = fragments.get(name) else {
                    continue;
                };"""
assert s.count(old) == 1
open(p, "w").write(s.replace(old, new))
