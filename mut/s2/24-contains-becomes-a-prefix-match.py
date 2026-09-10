# `_contains` must match anywhere, not only at the start.
p = "src/graph_query.rs"; s = open(p).read()
old = '            TextShape::Anywhere => format!("\'%{escaped}%\'"),'
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, '            TextShape::Anywhere => format!("\'{escaped}%\'"),'))
