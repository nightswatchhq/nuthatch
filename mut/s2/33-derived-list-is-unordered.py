# Drop the child ordering, so the aggregated array comes back in whatever order the scan produced.
p = "src/graph_query.rs"; s = open(p).read()
old = ' ORDER BY {alias}.\\"id\\" ASC LIMIT 100) t)'
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, ' LIMIT 100) t)'))
