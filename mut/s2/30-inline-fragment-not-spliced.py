# Drop an inline fragment's selections instead of splicing them.
p = "src/graph_query.rs"; s = open(p).read()
old = "            Sel::Inline(inner) => out.extend(resolve_spreads(inner, fragments, visiting)?),"
assert s.count(old) == 1
open(p, "w").write(s.replace(old, "            Sel::Inline(_) => {}"))
