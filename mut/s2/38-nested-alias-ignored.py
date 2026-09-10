# Shape a traversal's sub-fields under their real names rather than their aliases.
p = "src/graph_query.rs"; s = open(p).read()
old = "            sub.push((s.key.clone(), col));"
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, "            sub.push((s.name.clone(), col));"))
