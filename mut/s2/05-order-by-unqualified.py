p = "src/graph_query.rs"; s = open(p).read()
old = ' ORDER BY {BASE}.\\"{order_field}\\" {dir}'
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, ' ORDER BY \\"{order_field}\\" {dir}'))
