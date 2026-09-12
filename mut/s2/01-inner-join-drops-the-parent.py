p = "src/graph_query.rs"; s = open(p).read()
assert " LEFT JOIN" in s
open(p, "w").write(s.replace(" LEFT JOIN", " INNER JOIN"))
