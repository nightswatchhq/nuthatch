# Keep the EXISTS but drop the child's own conditions, so every parent with a present child matches.
p = "src/graph_query.rs"; s = open(p).read()
old = "            parts?.join(\" AND \")"
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, "            { let _ = parts?; \"TRUE\".to_string() }"))
